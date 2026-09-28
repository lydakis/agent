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
  config: null, ui: { rail: true, side: null, picker: false, pickerSel: 0, help: false, steps: false, toast: null, menu: false, folded: new Set() },
  // How Send reaches a working bot, the last pick from its menu (sticky across windows), and the
  // model each bot's next turns run on when it differs from the one it was created with.
  send: loadSend(), override: new Map(),
  // Each provider's model family, from the bot records that name both; a turn may run on any
  // provider of its bot's family.
  families: new Map(),
  // Unsent text for each bot not on screen. A composer's text is its bot's own: when a pane shows
  // another bot, the text stays behind with the one it was typed for (see `followDrafts`).
  drafts: new Map(),
};
function loadSend() { try { const v = localStorage.getItem('agent:send'); return v === 'steer' || v === 'side' ? v : 'queue'; } catch (_) { return 'queue'; } }
const sessionKey = () => `agent:${S.config?.socket}|${S.config?.workspace}`;
const bot = (name) => S.bots.get(name);
const transcript = (name) => { if (!S.transcripts.has(name)) S.transcripts.set(name, { items: [], nodes: 0, thoughts: 0, longOut: 0, peers: [], anchor: 'end', gen: 0, text: '', thinking: '', thinkingSince: 0, thinkingMs: 0, streamingTurn: null, streamGen: 0 }); return S.transcripts.get(name); };
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
  if (t.items.length > WINDOW + LAZY_ITEMS || t.bytes > DECODE_BYTES) evict(t);
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
// Every history union compares the last included node, not the raw cursor:
// snapshot cursors can be exclusive, while evicted nodes are inclusive.
const firstDecoded = t => t.items.findIndex(it => !['node','tool_stub','history','peer_gap','note_gap'].includes(it.kind));
const historyEnd = r => r.next - (r.exclusive ? 1 : 0);
function mergeHistoryRange(target, source) {
  if (historyEnd(source) > historyEnd(target)) {
    target.next = source.next; target.exclusive = source.exclusive;
  }
  target.min = target.min == null || source.min == null ? undefined : Math.min(target.min, source.min);
  target.seed ||= source.seed;
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
    lo = Math.max(0, firstDecoded(t));
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
      mergeHistoryRange(prev, it);
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
      const stub = {...it, kind: 'tool_stub'}; count(t, stub, 1); append(stub);
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
  for (const r of ranges) {
    if (previous && (r.min ?? 0) <= historyEnd(previous)) {
      mergeHistoryRange(previous, r);
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
  const t = transcript(record.name);
  if (t.seeded) {
    if (t.seedSession === S.session) return;
    t.seedSession = S.session;
    if (record.head <= t.seedHead) return;
    // Replay can omit any retained-event prefix after a disconnect. Rebuild
    // the covered cache from lineage rather than guessing which nodes are missing.
    // Keep peer/activity rows and nodes committed after this snapshot's head.
    t.items = t.items.flatMap(it => {
      if (it.kind === 'history') return historyEnd(it) > record.head ? [{...it,min:Math.max(it.min ?? 0,record.head + 1),seed:false}] : [];
      const node = it.kind === 'node' ? it.node : it.from;
      return node == null || node > record.head ? [it] : [];
    });
    t.nodes = t.thoughts = t.longOut = t.bytes = 0;
    for (const it of t.items) count(t, it, 1);
  }
  t.seeded = true; t.seedSession = S.session; t.seedHead = record.head;
  const ids = t.items.map(it => it.kind === 'node' ? it.node : it.from).filter(id => id != null);
  const first = ids.length && Math.min(...ids) <= record.head ? Math.min(...ids) : null;
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
  learnFamily(b, record);
  learnWorkspace(b, record);
  if (record.created_by) { b.parent = record.created_by; b.parentId = record.created_by_id ?? null; }
  S.bots.set(b.name, b);
  seedHistory(record);
}
// A new folder means its branch is read again, when the bot is next shown.
function learnWorkspace(b, record) {
  const ws = record.workspace ?? null;
  if (b.workspace !== ws) { b.workspace = ws; b.branch = undefined; }
}
// A bot keeps its folder, so the app names one only for a bot that has none.
const home = (b) => (b.workspace ? {} : { workspace: S.config.workspace });
// A bot in a linked git worktree shows the branch it works on; read once, when its head is first drawn.
function readBranch(b) {
  if (b.branch !== undefined) return;
  b.branch = null;
  const ws = b.workspace;
  if (!ws || !Daemon.branch) return;
  Daemon.branch(ws).then((branch) => { if (branch && b.workspace === ws && bot(b.name) === b) { b.branch = branch; render(); } }, () => {});
}
// Records carry the family; creation events do not, so a bot seated from one takes its provider's.
function learnFamily(b, record) {
  if (typeof record.family === 'string') { b.family = record.family; if (typeof record.provider === 'string') S.families.set(record.provider, record.family); }
  else b.family ??= S.families.get(record.provider) ?? null;
}
// The creator, when the bot holding that name now is the identity that did the creating. A later
// bot reusing the name is a stranger, and a creator the store could not resolve links to nothing.
function creatorOf(b) { const p = b.parent && b.parentId != null ? S.bots.get(b.parent) : null; return p && p.id === b.parentId ? p : null; }
function forgetBot(name) {
  const parent = bot(name) && creatorOf(bot(name));
  const t = parent && S.transcripts.get(parent.name);
  if (t) { t.items = t.items.filter(it => it.kind !== 'peer' || it.who !== name); t.peers = t.peers.filter(who => who !== name); t.gen += 1; }
  S.bots.delete(name); S.transcripts.delete(name); S.override.delete(name);
  // A draft belongs to its bot, so it goes with it.
  if (S.ui.side === name) S.ui.side = null;
  S.drafts.delete(name);
  for (const ids of Object.values(PANE)) { const input = $(ids.input); if (input.dataset.for === name) { input.value = ''; input.dataset.for = ''; } }
}
const ACTIVE = new Set(['running', 'waiting', 'paced', 'queued', 'ready']);
const isActive = (status) => ACTIVE.has(status);

// ---------- projects ----------
// A project is a folder, its coordinator bot `<project>.lead`, and `.agents/project.toml`. The list is
// the coordinator bots in the store, so nothing else can drift from it. A project's tasks are its
// coordinator's lineage, and any root bot named `<project>.<task>`.
const LEAD = '.lead';
const leadProject = (name) => name.length > LEAD.length && name.endsWith(LEAD) ? name.slice(0, -LEAD.length) : null;
function shortName(b) {
  const p = b?.project; if (!p) return b?.name ?? '';
  if (b.name === p + LEAD) return p;
  return b.name.startsWith(p + '.') ? b.name.slice(p.length + 1) : b.name;
}
// The sidebar's rows: each project's coordinator, then its tasks by lineage (unless folded), then
// the bots in no project. Rebuilt once per fleet shape change; it also stamps each bot's project.
// `all` includes folded projects' tasks, for finding rather than drawing.
function tree(all = false) {
  // One pass builds the children index; an explicit stack walks it, so a deep delegation chain
  // costs one prefix string per row and no recursion.
  const children = new Map(), projects = new Map();
  for (const b of S.bots.values()) {
    b.project = null;
    const key = creatorOf(b)?.name ?? null; if (!children.has(key)) children.set(key, []); children.get(key).push(b);
    const p = leadProject(b.name); if (p) projects.set(p, b);
  }
  // A root named `<project>.<task>` joins the longest project its name starts with.
  const prefixed = new Map();
  if (projects.size) for (const b of children.get(null) ?? []) {
    if (leadProject(b.name)) continue;
    for (let at = b.name.lastIndexOf('.'); at > 0; at = b.name.lastIndexOf('.', at - 1)) {
      const p = b.name.slice(0, at);
      if (projects.has(p)) { if (!prefixed.has(p)) prefixed.set(p, []); prefixed.get(p).push(b); break; }
    }
  }
  const out = []; const seen = new Set(); const stack = [];
  // Coordinators head their own projects wherever they were created.
  const pushKids = (parent, depth, cont, project, extra) => {
    const kids = [...(children.get(parent) ?? []), ...(extra ?? [])].filter((b) => !seen.has(b.name) && !leadProject(b.name));
    for (let i = kids.length - 1; i >= 0; i--) stack.push([kids[i], depth, i === kids.length - 1, cont, project]);
  };
  const walk = (hidden) => {
    let visited = 0;
    while (stack.length) {
      const [b, depth, last, cont, project] = stack.pop();
      if (seen.has(b.name)) continue; seen.add(b.name); b.project = project; visited++;
      const prefix = depth === 0 ? '' : cont + (last ? '└ ' : '├ ');
      if (!hidden) out.push({ b, depth, prefix });
      // The continuation stops growing past a few levels: a chain of thousands must not cost thousands per row.
      pushKids(b.name, depth + 1, depth === 0 ? '' : depth > 6 ? cont : cont + (last ? '  ' : '│ '), project);
    }
    return visited;
  };
  for (const p of [...projects.keys()].sort()) {
    const lead = projects.get(p); seen.add(lead.name); lead.project = p;
    const head = { b: lead, depth: 0, prefix: '', head: p, tasks: 0 }; out.push(head);
    pushKids(lead.name, 1, '', p, prefixed.get(p)); head.tasks = walk(!all && S.ui.folded.has(p));
  }
  const loose = out.length;
  pushKids(null, 0, '', null); walk(false);
  // Anything the roots do not reach is rooted where it stands: one pass, nothing hidden.
  for (const b of S.bots.values()) if (!seen.has(b.name)) { stack.push([b, 0, true, '', null]); walk(false); }
  if (projects.size && out.length > loose) out.splice(loose, 0, { label: 'bots' });
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
    case 'text_delta': { const t = transcript(name); t.streamingTurn = turn; if (t.thinkingSince) { t.thinkingMs += Date.now() - t.thinkingSince; t.thinkingSince = 0; } t.text += ev.text ?? ''; break; }
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
        if (typeof data.checkpoint === 'number') { t.history = { kind: 'history', next: data.checkpoint }; addItem(t, t.history); normalizeRanges(t); }
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
      if (b && !behindOwn) { b.status = data.status ?? 'queued'; b.runningTurn = turn; }
      addItem(transcript(name), { kind: 'note', text: behindOwn ? 'queued behind the running turn' : 'queued for a slot', turn });
      break;
    }
    case 'message': {
      const t = transcript(name);
      t.callNode = data.node;
      const thinkingSecs = t.streamingTurn === turn && (t.thinkingSince || t.thinkingMs) ? (t.thinkingMs + (t.thinkingSince ? Date.now() - t.thinkingSince : 0)) / 1000 : null;
      if (t.streamingTurn === turn) {
        // The committed node is the sole transcript source, including thinking.
        t.text = ''; t.thinking = ''; t.thinkingSince = 0; t.thinkingMs = 0; t.streamGen += 1;
      }
      if (typeof data.node === 'number') pushNode(t, { kind: 'node', node: data.node, turn, thinkingSecs });
      break;
    }
    case 'tool_started': {
      const args = data.arguments ?? '';
      let parsed = {}; try { parsed = JSON.parse(args) ?? {}; } catch (_) {}
      const tname = data.name ?? 'tool';
      const t = transcript(name);
      const row = { kind: 'tool', from: t.callNode, callId: data.call_id, name: tname, summary: callSummary(tname, args), background: tname === 'shell' && parsed.background === true, done: false, started: S.live ? Date.now() : 0, took: 0, turn };
      let existing = null;
      for (let i = t.items.length - 1; i >= 0; i--) {
        const it = t.items[i]; if (it.turn !== turn) break;
        if (it.kind === 'tool' && it.callId === data.call_id) { existing = it; break; }
      }
      if (existing) {
        row.from = existing.from ?? row.from;
        if (data.arguments_truncated) { row.summary = existing.summary; row.background = existing.background; }
        Object.assign(existing, row); t.gen += 1; } else addItem(t, row);
      break;
    }
    case 'tool_completed': {
      const t = transcript(name);
      let call = null;
      for (let i = t.items.length - 1; i >= 0; i--) { const it = t.items[i]; if ((it.kind === 'tool' || it.kind === 'tool_stub') && it.turn === turn && it.callId === data.call_id) { call = it; break; } }
      if (call) { call.done = true; if (call.started) call.took = Date.now() - call.started; call.started = 0; patchRun(name, call); }
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
      if (t.streamingTurn === turn) { t.text = ''; t.thinking = ''; t.thinkingSince = 0; t.thinkingMs = 0; t.streamingTurn = null; t.streamGen += 1; }
      if (status !== 'completed' && status !== 'steered') addItem(t, { kind: 'note', text: data.error ? `${status}: ${data.error}${data.detail ? ': ' + data.detail : ''}` : status, turn });
      // A background command may outlive its turn; only a wait result says how it ended.
      break;
    }
    case 'deleted': {
      if (S.snapshot) S.deleted.add(name);
      const p = bot(name)?.project;
      forgetBot(name);
      if (S.selected === name) S.selected = p && S.bots.has(p + LEAD) ? p + LEAD : S.bots.keys().next().value ?? '';
      break;
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
  const t = S.transcripts.get(name), session = S.session;
  let item; try { item = (await Daemon.request('history_items', { bot: name, nodes: [node] })).items[0]?.item; } catch (_) { return false; }
  if (!item || S.session !== session || S.transcripts.get(name) !== t) return false;
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
  return { kind: 'tool', name, callId, summary: callSummary(name, args), background: name === 'shell' && parsed.background === true, done: true, started: 0, took: 0 };
}
function entries(item) {
  const out = [];
  const text = (content, keys) => Array.isArray(content) ? content.filter((p) => keys.includes(p.type)).map((p) => p.text ?? '').join('') : typeof content === 'string' ? content : '';
  const shell = (o) => { let v; try { v = JSON.parse(o); } catch (_) { return o; } if (!v || typeof v !== 'object' || !('stdout' in v)) return o; let s = (v.stdout ?? '').trimEnd(); if (v.stderr?.trim()) s += (s ? '\n' : '') + 'stderr: ' + v.stderr.trimEnd(); if (v.exit_code) s += (s ? '\n' : '') + `exit ${v.exit_code}`; return s || '(no output)'; };
  // A failed call says so on its run's one line, so folding never hides a failure.
  const failure = (o) => { let v; try { v = JSON.parse(o); } catch (_) { return null; } if (!v || typeof v !== 'object') return null; if (v.error) return String(v.error); if (v.timed_out) return 'timed out'; if (typeof v.exit_code === 'number' && v.exit_code !== 0) return `exit ${v.exit_code}`; return v.success === false ? 'failed' : null; };
  const out_ = (callId, raw, isError) => ({ kind: 'out', callId, raw, text: shell(raw), err: failure(raw) ?? (isError ? 'error' : null) });
  if (item.type === 'function_call_output') return [out_(item.call_id, item.output ?? '')];
  if (item.type === 'function_call') return [storedTool(item.name, item.call_id, item.arguments)];
  if (item.type === 'reasoning') { const s = text(item.summary, ['summary_text']); if (s) out.push({ kind: 'thought', text: s, secs: null }); return out; }
  if (item.role === 'user') {
    if (Array.isArray(item.content)) { let t = ''; for (const p of item.content) { if (p.type === 'tool_result') out.push(out_(p.tool_use_id, text(p.content, ['text']), p.is_error === true)); else if (p.type === 'text' || p.type === 'input_text') t += p.text ?? ''; } if (t) out.unshift({ kind: 'user', text: t }); }
    else out.push({ kind: 'user', text: text(item.content, ['text']) });
  } else if (item.role === 'assistant' && Array.isArray(item.content)) {
    for (const p of item.content) {
      if (p.type === 'tool_use') out.push(storedTool(p.name, p.id, JSON.stringify(p.input)));
      else if (p.type === 'thinking' && p.thinking) out.push({ kind: 'thought', text: p.thinking, secs: null });
      else if ((p.type === 'text' || p.type === 'output_text') && p.text) {
        const previous = out.at(-1);
        if (previous?.kind === 'text') previous.text += p.text; else out.push({kind:'text',text:p.text});
      }
    }
  }
  return out;
}
async function loadInherited(name, older) {
  const t = S.transcripts.get(name); if (!t) return;
  const ranges = t.items.filter((it) => it.kind === 'history');
  if (!ranges.length) return;
  const first = firstDecoded(t);
  const marker = older ? ranges.find((r) => t.items.indexOf(r) <= Math.max(first, 0)) : ranges.findLast((r) => !r.loaded || (r.forward && t.anchor === 'end'));
  if (!marker || (!older && !marker.forward && !marker.seed && t.items.length > WINDOW)) return;
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
  else { const first = firstDecoded(t); if (first > 0) hi = first; }
  const pending = [];
  for (let i = hi - 1; i >= lo && pending.length < LAZY_ITEMS; i--) if (t.items[i].kind === 'node' || t.items[i].kind === 'tool_stub') pending.push([i, t.items[i]]);
  if (!pending.length) return false;
  const session = S.session;
  let progressed = false, bytes = 0;
  const ids = [...new Set(pending.filter(([,it]) => it.kind === 'node').map(([,it]) => it.node))];
  let fetched = new Map(), failure = null;
  if (ids.length) {
    try { const page = await Daemon.request('history_items', {bot:name,nodes:ids}); fetched = new Map(page.items.map(row => [row.node,row])); }
    catch (e) { failure = String(e?.message ?? e); }
  }
  const knownCalls = new Set(t.items.filter((it) => it.kind === 'tool' || it.kind === 'tool_stub').map((it) => JSON.stringify([it.turn, it.callId])));
  for (const [, it] of pending) {
    const index = t.items.indexOf(it); if (index < 0) continue;
    if (it.kind !== 'tool_stub' && !failure && !fetched.has(it.node)) continue;
    const row = fetched.get(it.node);
    const r = it.kind === 'tool_stub' ? {stub:true} : failure ? {err:failure} : row.error ? {err:row.error} : {ok:row.item};
    if (S.transcripts.get(name) !== t || S.session !== session) return false;
    if (r.stub) { count(t, it, -1); it.kind = 'tool'; t.gen += 1; progressed = true; continue; }
    // A lost session is not the item's fault: the node stays and the next attach fetches it. Anything else is final.
    if (r.err && /daemon_disconnected|detached|^io\b/.test(r.err)) break;
    progressed = true;
    const es = r.ok ? entries(r.ok) : [{ kind: 'note', text: `node ${it.node}: ${r.err}` }];
    const rep = [];
    let thinkingSecs = it.thinkingSecs;
    for (const e of es) {
      if (e.kind === 'thought' && thinkingSecs != null) { e.secs = thinkingSecs; thinkingSecs = null; }
      if (e.kind === 'tool' && it.turn != null && knownCalls.has(JSON.stringify([it.turn, e.callId]))) {
        const live = t.items.find(row => (row.kind === 'tool' || row.kind === 'tool_stub') && row.turn === it.turn && row.callId === e.callId);
        if (live) {
          // Preserve live timing, but the committed block owns its position.
          Object.assign(e, {started:live.started,took:live.took,done:live.done});
          count(t, live, -1); t.items.splice(t.items.indexOf(live), 1);
        }
      }
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
  if (S.ui.side && S.ui.side !== S.selected) await load(S.ui.side);
  // Cards on screen show their peer's last line, so those peers load too.
  for (const who of peers().slice(-12)) if (who !== S.selected && who !== S.ui.side) await load(who);
}

// All transcript mutations, including snapshot pages and creation replies,
// share the event/load queue. A snapshot must not replace ranges underneath
// a pending history read, and an old reply must not mutate a new session.
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
    try { await enqueue(async () => {
      for (const ev of batch.events ?? []) {
        await handle(ev, session, false);
        if (S.session !== session) return;
      }
      // One load and fleet rebuild per bounded pull, including replay bursts.
      if (S.live) await loadVisible();
      if (S.session === session) render();
    }); }
    catch (e) { if (S.session === session) lost(String(e?.message ?? e)); return; }
    if (batch.closed) { if (S.session === session) lost('the daemon closed the session'); return; }
  }
}
async function handle(ev, session, paint = true) {
  if (S.session !== session) return;
  const terminal = await onEvent(ev);
  if (S.session !== session) return;
  if (FLEET_EVENTS.has(ev.event)) { S.botsGen += 1; if (SHAPE_EVENTS.has(ev.event)) S.shapeGen += 1; else if (ev.bot) patchRailRow(ev.bot); }
  if (ev.bot && S.bots.has(ev.bot)) bot(ev.bot).touched = session;
  // During replay nothing is fetched: a load per node-producing event would serialize a long history
  // into one request each. The first load runs once follow_live arrives.
  if (!terminal && paint) { if (S.live) await loadVisible(); render(); }
}
function lost(reason) {
  S.lastReason = reason;
  S.session = null; S.attached = false; S.live = false;
  // Live deltas have no replay cursor. Reconnect rebuilds from durable nodes.
  for (const t of S.transcripts.values()) { t.text = ''; t.thinking = ''; t.thinkingSince = 0; t.thinkingMs = 0; t.streamingTurn = null; t.streamGen += 1; }
  showDetached(reason);
}
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
  learnFamily(b, record);
  learnWorkspace(b, record);
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
    // The login shell's model, looked up beside the attach so a slow profile never delays it, and
    // again on each attach while none is known (~/.agent/env may have been repaired meanwhile).
    if (!S.config.model) Daemon.defaultModel?.().then((m) => { if (m && !S.config.model) S.config.model = m; }, () => {});
    const { session } = await Daemon.attach(S.cursor);
    S.session = session;
    S.deleted = new Set(); S.snapshot = true;
    pump(session);
    // The snapshot, a page at a time, applied as it arrives while the replay flows.
    const listed = new Set(); let after = null;
    for (;;) {
      const page = await Daemon.request('bots', { after, limit: 256 });
      if (S.session !== session) return false;
      await enqueue(() => {
        if (S.session !== session) return;
        for (const record of page.bots ?? []) { listed.add(record.name); seat(record, session); }
      });
      if (S.session !== session) return false;
      if (!page.next_after) break;
      after = page.next_after;
    }
    await enqueue(async () => {
      if (S.session !== session) return;
      // Gone from the store while this page had no session: its live-only `deleted` notice cannot be
      // replayed. A bot this session's events mentioned was born after its page was listed, not deleted.
      for (const [name, b] of [...S.bots]) if (!listed.has(name) && b.touched !== session) { forgetBot(name); }
      // Resolve lineage only after all pages are seated: a child can sort before
      // its parent, and retention may have removed both creation events.
      for (const b of S.bots.values()) {
        const parent = creatorOf(b);
        if (parent) addItem(transcript(parent.name), {kind:'peer',who:b.name,turn:null});
      }
      S.snapshot = false; S.deleted.clear();
      S.attached = true;
      restore();
      // A selection deleted while detached had no `deleted` event to replay; show a surviving bot.
      if (!S.bots.has(S.selected)) { const first = tree()[0]; S.selected = first ? first.b.name : ''; }
      S.botsGen += 1; S.shapeGen += 1;
      await loadVisible();
      if (S.session !== session) return false;
    });
    if (S.session !== session) return false;
    $('detached').classList.remove('on');
    render();
    if (!S.setupSeen && !S.bots.size) { S.setupSeen = true; offerSetup(); }
    return true;
  } catch (e) {
    Daemon.log?.(`attach failed: ${e?.message ?? e}`);
    lost(String(e?.message ?? e));
    // Nothing to run yet: setup says what to bring, instead of an error.
    if (S.lastReason.startsWith('no_provider') && !S.setupSeen) openSetup();
    return false;
  }
}
function showDetached(reason) {
  S.attached = false;
  $('detached').innerHTML = `<div><b>not attached</b></div><div>${esc(reason)}</div><div style="margin-top:8px">daemon at <span class="k">${esc(S.config?.socket ?? '?')}</span> · retrying</div><div style="margin-top:12px"><button type="button" class="sbtn" data-act="settings">Open Settings</button></div>`;
  $('detached').classList.add('on');
  retryAttach();
}
function restore() {
  let saved = null; try { saved = JSON.parse(localStorage.getItem(sessionKey()) || 'null'); } catch (_) {}
  if (!saved) return;
  if (saved.selected && S.bots.has(saved.selected)) { S.selected = saved.selected; S.autoSelect = false; }
  if (saved.side && S.bots.has(saved.side) && saved.side !== S.selected) S.ui.side = saved.side;
  S.ui.rail = saved.rail !== false; S.ui.steps = !!saved.steps;
  if (Array.isArray(saved.folded)) { S.ui.folded = new Set(saved.folded.filter((p) => typeof p === 'string')); S.shapeGen += 1; }
  // A model pick belongs to the identity it was made for, not to whichever bot holds the name now.
  if (Array.isArray(saved.override)) for (const entry of saved.override) {
    const [name, id, model] = Array.isArray(entry) ? entry : [];
    const b = bot(name);
    if (b && b.id != null && b.id === id && typeof model === 'string' && runsOn(b, model)) S.override.set(name, model);
  }
}
function save() { try { localStorage.setItem(sessionKey(), JSON.stringify({ selected: S.selected, side: S.ui.side, rail: S.ui.rail, steps: S.ui.steps, folded: [...S.ui.folded], override: [...S.override].map(([name, model]) => [name, bot(name)?.id ?? null, model]) })); } catch (_) {} }
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
const moreButton = (name) => `<button type="button" class="ibtn" data-act="more" data-who="${esc(name)}" title="More" aria-label="More">⋯</button>`;
function cardInner({ status, name, last, elapsed, body }) {
  return `<span class="glyph ${status}" data-f="g">${glyphOf(status)}</span><span class="pn">${esc(name)}</span><span class="el" data-f="el">${elapsed ?? ''}</span><span class="pl" data-f="pl">${esc(last)}</span>${body ?? ''}`;
}
function cardHTML(card) {
  return `<div class="peer${card.sel ? ' sel' : ''}" ${card.attr}${card.task ? ' role="button" tabindex="0"' : ''}>${cardInner(card)}${card.task ? `<span class="tacts">${moreButton(card.task)}</span>` : ''}</div>`;
}
const cssEsc = (s) => String(s).replace(/[\x00-\x1f\x7f"\\]/g, (c) => c === '\0' ? '\ufffd' : c === '"' || c === '\\' ? '\\' + c : '\\' + c.charCodeAt(0).toString(16) + ' ');
// A task's card: its status, its elapsed time and its newest line. Click opens it beside.
function taskCard(who) {
  const p = bot(who); if (!p) return null;
  const el = p.turnStarted ? fmt(Date.now() - p.turnStarted) : p.elapsed ? fmt(p.elapsed) : '';
  return { attr: `data-task="${esc(p.name)}"`, task: p.name, status: p.status, name: shortName(p), last: lastLine(transcript(who)), elapsed: el, sel: S.ui.side === who || S.selected === who };
}
// The two panes that show a transcript: the main thread and the one beside it.
const PANES = [['log', () => S.selected], ['side', () => S.ui.side]];
const paneKey = (name, t) => `${name}|${t.gen}|${S.ui.steps}`;
// Replace one rendered card in place, in whichever pane shows that bot, so a process ending costs the
// size of its own card, not a rebuild of the window.
function patchItem(name, selector, it) {
  for (const [id, shown] of PANES) {
    if (shown() !== name) continue;
    const old = $(id).querySelector(selector);
    if (old) old.outerHTML = itemHTML(it);
  }
}
// A run's line is redrawn in place when one of its calls finishes. Only a run already on screen, and
// only as far as its pane has rendered; the next render appends the rest.
function patchRun(name, it) {
  const t = S.transcripts.get(name); if (!t) return;
  for (const [id, shown] of PANES) {
    if (shown() !== name) continue;
    const el = $(id); if (el.dataset.key !== paneKey(name, t)) continue;
    const len = Number(el.dataset.len), i = t.items.lastIndexOf(it);
    if (i < 0 || i >= len) continue;
    const s = runStart(t, i), old = el.querySelector(`.steps[data-i="${s}"]`);
    if (old) old.outerHTML = runHTML(t, s, len).html;
  }
}
// What changes with time, refreshed in place: task cards (their bot's status and last line), and
// running tools' elapsed. Fields change, not the card, so a card under the pointer is never replaced.
function refreshLive(el) {
  for (const card of el.querySelectorAll('.peer[data-task]')) {
    const c = taskCard(card.dataset.task); if (!c) continue;
    card.classList.toggle('sel', c.sel);
    const g = card.querySelector('[data-f="g"]'); if (g) { g.className = `glyph ${c.status}`; g.textContent = glyphOf(c.status); }
    const e = card.querySelector('[data-f="el"]'); if (e) e.textContent = c.elapsed;
    const l = card.querySelector('[data-f="pl"]'); if (l) l.textContent = c.last;
  }
  for (const span of el.querySelectorAll('.el[data-started]')) span.textContent = fmt(Date.now() - Number(span.dataset.started));
}
// A card's one line: the last non-empty line of the newest text, bounded, so a long reply costs the
// parent's render nothing.
function tailOf(s) { const end = s.slice(-400).trimEnd(); const at = end.lastIndexOf('\n'); return end.slice(at + 1).trim().slice(0, 200); }
function lastLine(t) {
  if (t.text) return tailOf(t.text); if (t.thinking) return t.thinking.slice(-400).split('. ').pop().slice(0, 200);
  for (let i = t.items.length - 1; i >= 0; i--) { const it = t.items[i]; if (it.kind === 'text') return tailOf(it.text); if (it.kind === 'tool') return `▸ ${it.name} ${it.summary}`; }
  return '';
}

// ---------- runs ----------
// Thinking, tool calls and their output between two messages form one run of steps. A run is one
// line, folded until clicked: the call in progress, or the tools it used, and any failure.
const STEP = new Set(['thought', 'tool', 'out']);
const inRun = (it, turn) => (STEP.has(it.kind) || it.kind === 'backing') && it.turn === turn;
function runStart(t, i) {
  const turn = t.items[i].turn; let s = i;
  while (s > 0 && inRun(t.items[s - 1], turn)) s--;
  while (s < i && !STEP.has(t.items[s].kind)) s++;
  return s;
}
function runEnd(t, s, limit = t.items.length) { const turn = t.items[s].turn; let e = s + 1; while (e < limit && inRun(t.items[e], turn)) e++; return e; }
function runHTML(t, s, limit = t.items.length) {
  const end = runEnd(t, s, limit), items = t.items.slice(s, end);
  const tools = items.filter((it) => it.kind === 'tool'), thoughts = items.filter((it) => it.kind === 'thought');
  const open = S.ui.steps || items.some((it) => it.runOpen);
  const n = tools.length + thoughts.length;
  // Output whose call is not loaded yet shows as output, not as a fold labeled thought.
  if (!n) return { html: `<div class="steps" data-i="${s}"><div class="body">${items.map((it, k) => stepHTML(it, s + k)).join('')}</div></div>`, end };
  let head;
  if (!tools.length) {
    const secs = thoughts.length && thoughts.every((it) => it.secs != null) ? thoughts.reduce((a, it) => a + it.secs, 0) : null;
    head = `thought${secs == null ? '' : ' ' + fmt(secs * 1000)}`;
  } else {
    const last = tools[tools.length - 1];
    const el = last.started ? ` <span class="el" data-started="${last.started}">${fmt(Date.now() - last.started)}</span>` : last.took >= 1500 ? ` <span class="el">${fmt(last.took)}</span>` : '';
    const now = n === 1 || last.started ? `<b>${esc(last.name)}</b> ${esc(last.summary)}${el}` : esc([...new Set(tools.map((it) => it.name))].join(' · '));
    head = n === 1 ? `<span class="now">${now}</span>` : `${n} steps <span class="now">${now}</span>`;
  }
  let err = null; for (const it of items) if (it.kind === 'out' && it.err) err = it.err;
  if (err) head += ` <span class="err">✘ ${esc(err)}</span>`;
  const body = open ? `<div class="body">${items.map((it, k) => stepHTML(it, s + k)).join('')}</div>` : '';
  return { html: `<div class="steps" data-i="${s}"><div class="sum" data-run="${s}" role="button" tabindex="0">${open ? '▾' : '▸'} ${head}</div>${body}</div>`, end };
}
function stepHTML(it, i) {
  switch (it.kind) {
    case 'thought': return `<div class="line think">${esc(it.text)}</div>`;
    case 'tool': { const el = it.started ? `<span class="el" data-started="${it.started}">${fmt(Date.now() - it.started)}</span>` : it.took >= 1500 ? `<span class="el">${fmt(it.took)}</span>` : ''; return `<div class="line tool" data-call="${esc(it.callId)}">▸ <b>${esc(it.name)}</b> ${esc(it.summary)}${el}</div>`; }
    case 'out': {
      const rows = it.text.split('\n').filter((l) => l.trim()); const long = rows.length > 2;
      const shown = long && !S.ui.steps && !it.open ? rows.slice(0, 2) : rows;
      return `<div class="line out${long ? ' fold' : ''}${it.err ? ' bad' : ''}"${long ? ` data-out="${i}"` : ''}>${esc(shown.join('\n'))}${shown.length < rows.length ? ` <span class="more">+${rows.length - shown.length} lines</span>` : ''}</div>`;
    }
    default: return '';
  }
}
// Open or close a run, or unfold one output, and redraw just that run.
function toggleStep(target) {
  const pane = target.closest('.scroll'); if (!pane) return;
  const name = pane.id === 'side' ? S.ui.side : S.selected, t = name && S.transcripts.get(name);
  if (!t || pane.dataset.key !== paneKey(name, t)) return;
  const len = Number(pane.dataset.len);
  const i = Number(target.dataset.out ?? target.dataset.run); if (!(i >= 0 && i < len)) return;
  const s = runStart(t, i), end = runEnd(t, s, len);
  if (target.dataset.out != null) t.items[i].open = !t.items[i].open;
  else { const open = !t.items.slice(s, end).some((it) => it.runOpen); for (let k = s; k < end; k++) t.items[k].runOpen = open; }
  const old = pane.querySelector(`.steps[data-i="${s}"]`); if (old) old.outerHTML = runHTML(t, s, len).html;
}

function itemHTML(it) {
  switch (it.kind) {
    case 'user': return `<div class="line user">› ${esc(it.text)}</div>`;
    case 'text': return markdown(it.text);
    case 'note': return `<div class="line note">${esc(it.text)}</div>`;
    case 'note_gap': return `<div class="line note">${it.total} ${it.later ? 'later' : 'earlier'} activity notes summarized · durable messages remain available</div>`;
    case 'peer_gap': return `<div class="line note">${it.total} earlier tasks · ^k finds a bot</div>`;
    case 'peer': { const c = taskCard(it.who); return c ? cardHTML(c) : ''; }
    case 'proc': { const status = it.done === null ? 'running' : 'idle'; const last = it.done === null ? it.handle : (it.done || 'done'); return cardHTML({ attr: `data-proc="${esc(it.handle)}"`, status, name: `$ ${it.cmd}`, last, elapsed: '', sel: false }); }
    default: return STEP.has(it.kind) ? stepHTML(it, -1) : '';
  }
}
// The items from `from` on; a blank line separates turns, judged against the nearest earlier item with
// one. A run of bare nodes is one placeholder row, so unloaded history costs one element per gap, and a
// run of steps is one line.
function itemsHTML(t, from = 0) {
  let h = ''; let lastTurn = null; let gap = 0;
  for (let i = from - 1; i >= 0; i--) if (t.items[i].turn != null) { lastTurn = t.items[i].turn; break; }
  const flush = () => { if (gap) { h += `<div class="line pending">… ${gap} earlier</div>`; gap = 0; } };
  for (let i = from; i < t.items.length;) {
    const it = t.items[i];
    if (it.kind === 'history') { flush(); h += `<div class="line pending">… ${it.forward ? 'later history · scroll down' : 'earlier history · scroll up'} to load</div>`; i++; continue; }
    if (it.kind === 'node' || it.kind === 'tool_stub') { gap += 1; i++; continue; }
    flush();
    if (it.turn != null && it.turn !== lastTurn) { if (h || from > 0) h += `<div class="line sep" data-sep="${i}"></div>`; lastTurn = it.turn; }
    if (STEP.has(it.kind)) { const run = runHTML(t, i); h += run.html; i = run.end; continue; }
    h += itemHTML(it); i++;
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
// A streamed delta touches only the tail. The items rebuild when a load replaced nodes (gen) or the
// steps fold changes; items appended since the last render are added on their own, and a step that
// extends the run on screen redraws that run alone.
function renderTranscript(el, name) {
  const t = S.transcripts.get(name);
  // Another bot in this pane starts at its newest work, not at the old bot's scroll position.
  const fresh = el.dataset.who !== name; el.dataset.who = name;
  const atBottom = fresh || el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  const before = el.scrollHeight;
  if (!t) { el.innerHTML = ''; el.dataset.key = ''; return; }
  const key = paneKey(name, t);
  const rendered = el.dataset.key === key ? Number(el.dataset.len) : -1;
  let tail = el.lastElementChild;
  if (rendered < 0 || rendered > t.items.length || !tail || !tail.classList.contains('tail')) {
    el.innerHTML = itemsHTML(t) + '<div class="tail"></div>';
    el.dataset.key = key;
    tail = el.lastElementChild;
    // History loaded above the reader keeps their place instead of shoving it down.
    if (!atBottom) el.scrollTop += el.scrollHeight - before;
  } else if (rendered < t.items.length) {
    let from = rendered;
    const next = t.items[rendered], prev = t.items[rendered - 1];
    if (prev && inRun(prev, next.turn) && inRun(next, next.turn)) {
      const s = runStart(t, rendered - 1);
      const old = STEP.has(t.items[s].kind) ? el.querySelector(`.steps[data-i="${s}"]`) : null;
      if (old) { const sep = old.previousElementSibling; if (sep?.dataset?.sep === String(s)) sep.remove(); old.remove(); from = s; }
    }
    tail.insertAdjacentHTML('beforebegin', itemsHTML(t, from));
  }
  el.dataset.len = String(t.items.length);
  refreshLive(el);
  renderTail(tail, name, t);
  if (atBottom) el.scrollTop = el.scrollHeight;
}
for (const [id, who] of PANES) {
  $(id).addEventListener('scroll', () => {
    const el = $(id); const name = who(); const t = name && S.transcripts.get(name); if (!t) return;
    const nearTop = el.scrollTop < 200, nearEnd = el.scrollHeight - el.scrollTop - el.clientHeight < 200;
    // The window follows the reader: the edge they reach is where bodies stay decoded.
    if (nearTop && !nearEnd) t.anchor = 'top'; else if (nearEnd) t.anchor = 'end';
    if ((nearTop || nearEnd) && (t.nodes > 0 || t.history != null)) enqueue(async () => { await load(name, nearTop); render(); });
  });
}

// ---------- heads and composers ----------
function headHTML(b, pane) {
  const waiting = b.waitingOn.length ? ` on ${esc(waitSummary(b))}` : '';
  const state = `<span class="glyph ${b.status}">${glyphOf(b.status)}</span><span class="state">${labelOf(b.status)}${waiting}</span>`;
  if (pane === 'side') return `<div class="crumbs"><b>${esc(shortName(b))}</b>${branchHTML(b)}${state}</div><div class="tools">${moreButton(b.name)}<button type="button" class="ibtn" data-act="swap" title="Full view" aria-label="Full view">⤢</button><button type="button" class="ibtn" data-act="close-side" title="Close (Esc)" aria-label="Close">✕</button></div>`;
  const lead = b.project ? bot(b.project + LEAD) : null;
  const crumbs = !lead ? `<b>${esc(b.name)}</b>` : lead === b ? `<b>${esc(b.project)}</b>`
    : `<button type="button" class="back" data-act="open" data-who="${esc(lead.name)}" title="Back to the coordinator">← ${esc(b.project)}</button><span class="sep">/</span><b>${esc(shortName(b))}</b>`;
  return `<div class="crumbs">${crumbs}${branchHTML(b)}${state}</div><div class="tools">${moreButton(b.name)}</div>`;
}
const branchHTML = (b) => (b.branch ? `<span class="branch" title="${esc(b.workspace)}">⎇ ${esc(b.branch)}</span>` : '');
// A head names a few of the handles a bot waits on and counts the rest, so its cost stays bounded.
const WAIT_SHOWN = 3;
function waitSummary(b) {
  const shown = b.waitingOn.slice(0, WAIT_SHOWN).map((h) => h.replace(/^turn:/, '')).join(', ');
  const more = b.waitingOn.length - WAIT_SHOWN;
  return more > 0 ? `${shown} +${more}` : shown;
}
// Heads change with their bot's status, not with time, so they are written only when that changes.
function renderHead(el, b, pane) {
  if (b) readBranch(b);
  const key = b ? `${b.name}|${b.status}|${waitSummary(b)}|${b.project}|${b.branch ?? ''}` : '-';
  if (el.dataset.k === key) return; el.dataset.k = key;
  el.innerHTML = b ? headHTML(b, pane) : pane === 'main' ? '<div class="crumbs"><span class="state">no bots · /new NAME creates one</span></div>' : '';
}
const PANE = {
  main: { form: 'form', input: 'input', model: 'model', send: 'send', stop: 'stop', bot: () => S.selected },
  side: { form: 'sideform', input: 'sideinput', model: 'sidemodel', send: 'sidesend', stop: 'sidestop', bot: () => S.ui.side },
};
const ACTION = { send: 'Send', queue: 'Queue', steer: 'Steer', side: 'Side chat' };
// Send starts a turn on a bot at rest; on a working bot it does what the menu last picked.
// Only a turn that has started can take a steer; one waiting for a slot takes a queue.
const RUNNING = new Set(['running', 'waiting', 'paced']);
const sendMode = (b) => !b || !isActive(b.status) ? 'send' : S.send === 'steer' && !RUNNING.has(b.status) ? 'queue' : S.send;
const providerOf = (model) => String(model ?? '').split('/')[0];
// A turn may run on the bot's own provider, or on another the fleet's records show in its family.
function runsOn(b, model) {
  const p = providerOf(model), own = providerOf(b.model);
  if (p === own) return true;
  const fam = b.family ?? S.families.get(own);
  return fam != null && S.families.get(p) === fam;
}
const modelOf = (b) => S.override.get(b.name) ?? b.model;
function renderComposer(pane, b) {
  const ids = PANE[pane], mode = sendMode(b), model = b ? modelOf(b) : '';
  const key = b ? `${b.name}|${mode}|${model}|${b.runningTurn !== null}` : '-';
  const send = $(ids.send); if (send.dataset.k === key) return; send.dataset.k = key;
  send.textContent = ACTION[mode];
  $(ids.model).textContent = b ? `${model.split('/').slice(1).join('/') || model} ▾` : '';
  $(ids.model).hidden = !b; $(ids.stop).hidden = !b || b.runningTurn === null;
  $(ids.input).placeholder = !b ? (pane === 'main' ? '/new NAME [PROVIDER/MODEL]' : '') : mode === 'queue' ? 'queues after this turn' : mode === 'steer' ? 'steers into this turn' : mode === 'side' ? 'asks a side chat' : '';
}

// ---------- sidebar ----------
// The sidebar shows a window of rows around the selection; scrolling to an edge extends it. The rows
// are rebuilt when the fleet's shape changes (a bot created, forked or deleted, a project folded); a
// status change patches the bot's own row. A fleet of thousands costs a screenful of rows, not a row
// each per event.
const RAIL_ROWS = 300;
const rail = { shapeGen: -1, drawn: -1, selected: null, rows: [], index: new Map(), start: 0, end: 0, key: '' };
function railRows() {
  if (rail.shapeGen !== S.shapeGen) {
    rail.rows = tree(); rail.index = new Map(); rail.rows.forEach((n, i) => { if (n.b) rail.index.set(n.b.name, i); });
    rail.shapeGen = S.shapeGen; rail.key = '';
  }
  return rail.rows;
}
function renderRail() {
  const recenter = rail.drawn !== S.shapeGen || rail.selected !== S.selected;
  const rows = railRows(); const el = $('bots');
  const sel = rail.index.get(S.selected) ?? 0;
  const key = () => `${S.shapeGen}|${S.selected}|${S.ui.side}|${rail.start}|${rail.end}`;
  if (rail.key !== key()) {
    if (recenter) { rail.start = Math.max(0, sel - RAIL_ROWS / 2); rail.end = Math.min(rows.length, rail.start + RAIL_ROWS); }
    rail.selected = S.selected; rail.drawn = S.shapeGen;
    rail.key = key();
    const above = rail.start ? `<div class="botrow more">… ${rail.start} above</div>` : '';
    const below = rail.end < rows.length ? `<div class="botrow more">… ${rows.length - rail.end} below</div>` : '';
    el.innerHTML = above + rows.slice(rail.start, rail.end).map((n) => botRowHTML(n, n.b?.name === S.selected)).join('') + below;
    el.dataset.key = rail.key;
  }
}
// A bot's row, replaced in place when its status changes; nothing if it is outside the window.
function patchRailRow(name) {
  if (!S.ui.rail) return;
  const i = rail.index.get(name); if (i === undefined || i < rail.start || i >= rail.end) return;
  const el = $('bots'); const old = el.querySelector(`.botrow[data-bot="${cssEsc(name)}"]`); if (!old) return;
  old.outerHTML = botRowHTML(rail.rows[i], name === S.selected);
}
$('bots').addEventListener('scroll', () => {
  const el = $('bots'); const rows = rail.rows, previousStart = rail.start; let moved = false;
  if (el.scrollTop < 100 && rail.start > 0) { rail.start = Math.max(0, rail.start - RAIL_ROWS / 2); moved = true; }
  else if (el.scrollHeight - el.scrollTop - el.clientHeight < 100 && rail.end < rows.length) { rail.start = Math.min(Math.max(0, rows.length - RAIL_ROWS), rail.start + RAIL_ROWS / 2); moved = true; }
  if (moved) {
    rail.end = Math.min(rows.length, rail.start + RAIL_ROWS);
    const name = rows.slice(Math.max(previousStart, rail.start)).find((n) => n.b)?.b.name;
    const selector = `.botrow[data-bot="${cssEsc(name)}"]`;
    const before = el.querySelector(selector)?.offsetTop, top = el.scrollTop;
    rail.key = ''; renderRail();
    const after = el.querySelector(selector)?.offsetTop;
    if (before != null && after != null) el.scrollTop = top + after - before;
  }
});
// A project's row is its coordinator: click it to talk to the coordinator; the chevron folds its tasks.
function botRowHTML(n, sel) {
  if (n.label) return `<div class="sblabel">${esc(n.label)}</div>`;
  const b = n.b, acts = `<span class="acts">${moreButton(b.name)}</span>`;
  const glyph = `<span class="glyph ${b.status}">${glyphOf(b.status)}</span>`;
  if (n.head != null) {
    const folded = S.ui.folded.has(n.head);
    const chev = n.tasks ? `<button type="button" class="chev" data-act="fold" data-v="${esc(n.head)}" aria-label="${folded ? 'Show' : 'Hide'} tasks">${folded ? '▸' : '▾'}</button>` : '<span class="chev"></span>';
    return `<div class="botrow proj${sel ? ' sel' : ''}" data-bot="${esc(b.name)}" role="button" tabindex="0">${chev}${glyph}<span class="n">${esc(n.head)}</span>${acts}</div>`;
  }
  const beside = S.ui.side === b.name ? ' beside' : '';
  return `<div class="botrow${sel ? ' sel' : ''}${beside}" data-bot="${esc(b.name)}" role="button" tabindex="0"><span class="tree">${n.prefix}</span>${glyph}<span class="n">${esc(shortName(b))}</span>${acts}</div>`;
}
function peers() { return (S.transcripts.get(S.selected)?.peers ?? []).filter((who) => S.bots.has(who)); }
function keybarHTML(b) {
  const busy = b && isActive(b.status);
  const dot = `<span><span class="dot${!S.attached ? ' off' : busy ? ' busy' : ''}"></span>${!S.attached ? 'detached' : 'live'}</span>`;
  const keys = [];
  if (S.ui.picker) keys.push('<kbd>↑↓</kbd> choose', '<kbd>Enter</kbd> open', '<kbd>Esc</kbd> cancel');
  else { keys.push('<kbd>^k</kbd> find'); if (S.ui.side) keys.push('<kbd>Esc</kbd> close'); else if (busy) keys.push('<kbd>Esc</kbd> stop'); }
  return `${dot}${S.ui.toast ? `<span class="toast">${esc(S.ui.toast)}</span>` : ''}<span class="spacer"></span>${keys.join('<span> </span>')}<span><kbd>?</kbd> keys</span>`;
}
// Each composer holds the text of the bot its pane shows. When a pane shows another bot, its text is
// put away under the bot it was typed for, then the new bot's comes back: every pane is put away
// before any is filled, so a swap trades the two texts.
function followDrafts() {
  const moved = [];
  for (const ids of Object.values(PANE)) {
    const input = $(ids.input), who = ids.bot() ?? '';
    if ((input.dataset.for ?? '') === who) continue;
    if (input.dataset.for) { if (input.value) S.drafts.set(input.dataset.for, input.value); else S.drafts.delete(input.dataset.for); }
    moved.push([input, who]);
  }
  for (const [input, who] of moved) { input.value = S.drafts.get(who) ?? ''; S.drafts.delete(who); input.dataset.for = who; grow(input); }
}
function render() {
  const app = $('app');
  // The sidebar's rows also stamp each bot's project, which names and crumbs use.
  railRows();
  const b = bot(S.selected);
  if (S.ui.side && (!S.bots.has(S.ui.side) || S.ui.side === S.selected)) S.ui.side = null;
  const side = S.ui.side ? bot(S.ui.side) : null;
  app.classList.toggle('rail', S.ui.rail); app.classList.toggle('side', !!side);
  followDrafts();
  renderHead($('title'), b, 'main');
  if (b) renderTranscript($('log'), b.name); else { $('log').innerHTML = ''; $('log').dataset.key = ''; }
  if (S.ui.rail) renderRail();
  if (side) { renderHead($('sidetitle'), side, 'side'); renderTranscript($('side'), side.name); }
  renderComposer('main', b); renderComposer('side', side);
  $('keybar').innerHTML = keybarHTML(b);
  if (S.ui.picker) renderPicker();
  refreshMenu();
}
// Once a second, while anything runs: the clocks on cards and run lines, in place. The activity
// check is cached per fleet change, so a quiet fleet of any size costs nothing here.
let activeAt = -1, active = false;
function anyActive() { if (activeAt !== S.botsGen) { activeAt = S.botsGen; active = [...S.bots.values()].some((b) => isActive(b.status)); } return active; }
setInterval(() => { if (S.attached && anyActive()) { refreshLive($('log')); if (S.ui.side) refreshLive($('side')); } }, 1000);

// ---------- picker ----------
function pickerRows() {
  const q = $('pickerq').value.trim().toLowerCase();
  return tree(true).filter((n) => n.b).map((n) => ({ ...n, i: q ? n.b.name.toLowerCase().indexOf(q) : -1 })).filter((r) => !q || r.i >= 0);
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
let pickerPane = 'main';
function openPicker() { pickerPane = paneOf(document.activeElement, menuPane); closeMenu(); S.ui.picker = true; S.ui.pickerSel = 0; $('pickerq').value = ''; $('pickerwrap').classList.add('on'); render(); $('pickerq').focus(); }
// A pick opens its bot alone, so focus goes to the main composer; Escape goes back where it was.
function closePicker(pane = pickerPane) { S.ui.picker = false; $('pickerwrap').classList.remove('on'); render(); $(PANE[S.ui.side ? pane : 'main'].input).focus(); }
let helpPane = 'main';
async function showHelp(pane = 'main') {
  helpPane = pane;
  // Open at once so Esc closes it; the list is read now, so an edited ~/.agent/models shows without a restart.
  const shown = S.ui.help = {}; const text = (models) => { $('helptext').innerHTML = `<b>keys</b>\n ^k   find a bot        ^b   sidebar\n ^p   next task beside  Esc  close beside · stop\n ^o   all steps         ^d   detach (close)\n ↑ ↓  previous / next bot   ^,   settings\n Enter sends · Shift-Enter a new line\n\n /new NAME [PROVIDER/MODEL]   create a bot\n${models}\n<i>any key closes this</i>`; };
  text('   reading ~/.agent/models'); $('helpwrap').classList.add('on');
  let models; try { const list = await Daemon.models(); models = list.length ? list.map((m) => `   ${esc(m.id)}`).join('\n') : '   none listed: Settings lists your providers\' models'; } catch (e) { models = `   ${esc(String(e?.message ?? e))}`; }
  if (S.ui.help === shown) text(models);
}
function hideHelp() { S.ui.help = false; $('helpwrap').classList.remove('on'); $(PANE[S.ui.side ? helpPane : 'main'].input).focus(); }

// ---------- menus ----------
// One menu at a time: an agent's ⋯ (from its head, its sidebar row, its card, or a right-click), the
// model chip, and Send's ▾.
let menuAnchor = null, menuFor = null, menuKey = '', menuPane = 'main';
const menuSig = (items) => items.map((i) => `${i.act}:${!!i.disabled}:${!!i.on}:${i.hint ?? ''}`).join('|');
function menuHTML(items) {
  return items.map((i) => i.sep ? '<hr>' : i.head ? `<div class="lab">${esc(i.head)}</div>` :
    `<button type="button" role="menuitem"${i.disabled ? ' disabled' : ''}${i.danger ? ' class="danger"' : ''} data-act="${i.act}"${i.who != null ? ` data-who="${esc(i.who)}"` : ''}${i.v != null ? ` data-v="${esc(i.v)}"` : ''}${i.pane ? ` data-pane="${i.pane}"` : ''}><span><span class="ck">${i.on ? '✓' : ''}</span>${esc(i.label)}</span><span class="mh">${esc(i.hint ?? '')}</span></button>`).join('');
}
function showMenu(items, anchor, who = null) {
  // Focus goes back to the pane the menu came from, so typing after it reaches the same bot.
  menuPane = paneOf(document.activeElement, menuPane);
  const m = $('menu'); m.innerHTML = menuHTML(items); m.classList.add('on'); S.ui.menu = true; menuAnchor = anchor; menuFor = who; menuKey = menuSig(items);
  const w = m.offsetWidth, h = m.offsetHeight, W = window.innerWidth, H = window.innerHeight;
  let x = anchor.x, y = anchor.y;
  if (anchor.rect) { const a = anchor.rect; x = a.right - w; y = anchor.up ? a.top - h - 4 : a.bottom + 4; }
  m.style.left = `${Math.max(4, Math.min(x, W - w - 4))}px`; m.style.top = `${Math.max(4, Math.min(y, H - h - 4))}px`;
  m.querySelector('button:not([disabled])')?.focus({ preventScroll: true });
}
function closeMenu() {
  if (!S.ui.menu) return; S.ui.menu = false; menuFor = null;
  const m = $('menu'), had = m.contains?.(document.activeElement); m.classList.remove('on');
  // A menu that replaces this one (Delete's confirmation) keeps the focus.
  if (had) setTimeout(() => { if (!S.ui.menu && !S.ui.picker) $(PANE[S.ui.side ? menuPane : 'main'].input)?.focus({ preventScroll: true }); }, 0);
}
// The composer pane an element sits in; focus inside the menu keeps the pane it came from.
function paneOf(el, fallback = 'main') {
  if ($('menu').contains?.(el)) return fallback;
  return el?.closest?.('.pane.side') ? 'side' : 'main';
}
// An open agent menu follows its bot: a status change rebuilds it in place, a deletion closes it.
function refreshMenu() {
  if (!S.ui.menu || menuFor == null) return;
  if (!S.bots.has(menuFor)) { closeMenu(); return; }
  const items = botMenuItems(menuFor);
  if (menuSig(items) !== menuKey) showMenu(items, menuAnchor, menuFor);
}
// Keep, which turns a side chat into a task, is not built yet.
function botMenuItems(name) {
  const b = bot(name); if (!b) return [];
  const busy = isActive(b.status);
  return [
    { act: 'side-chat', who: name, label: 'Side chat', hint: '⑂', disabled: b.id == null },
    { act: 'stop', who: name, label: 'Stop', disabled: b.runningTurn === null },
    { act: 'fork', who: name, label: 'Fork', hint: busy ? 'when idle' : '', disabled: busy },
    { act: 'delete', who: name, label: 'Delete', hint: busy ? 'when idle' : '', disabled: busy },
    { sep: true },
    { act: 'steps', label: 'Show all thoughts and output', on: S.ui.steps, hint: '^o' },
  ];
}
// A bot keeps its model family for life; a turn may run on any model of that family. Another family,
// or a provider whose family no record shows, is a new agent. The bot's own model is always offered.
function modelChoices(b, list) {
  const ids = list.map((m) => m.id);
  const mine = ids.filter((id) => runsOn(b, id)), others = ids.filter((id) => !runsOn(b, id));
  if (!mine.includes(b.model)) mine.unshift(b.model);
  const current = modelOf(b);
  return [...mine.map((id) => ({ id, ok: true, on: id === current })), ...others.map((id) => ({ id, ok: false, on: false }))];
}
function modelMenuItems(b, list, error) {
  // Each provider under its own heading, so the menu says where a model comes from.
  const items = []; let group = null;
  for (const c of modelChoices(b, list)) {
    const p = providerOf(c.id);
    if (p !== group) { if (group !== null) items.push({ sep: true }); items.push({ head: providerLabel(p) }); group = p; }
    items.push({ act: 'set-model', who: b.name, v: c.id, label: c.id.slice(p.length + 1), on: c.on, disabled: !c.ok, hint: c.ok ? '' : 'new agent' });
  }
  if (error) items.push({ sep: true }, { act: 'none', label: error, disabled: true });
  return items;
}
function sendMenuItems(pane) {
  return [
    { act: 'set-send', pane, v: 'queue', label: 'Queue after this turn', on: S.send === 'queue' },
    { act: 'set-send', pane, v: 'steer', label: 'Steer into this turn', on: S.send === 'steer' },
    { act: 'set-send', pane, v: 'side', label: 'Ask a side chat', hint: '⑂', on: S.send === 'side' },
  ];
}
function setSend(mode) { S.send = mode === 'steer' || mode === 'side' ? mode : 'queue'; try { localStorage.setItem('agent:send', S.send); } catch (_) {} }
function setModel(name, model) {
  const b = bot(name); if (!b || !runsOn(b, model)) return false;
  if (model === b.model) S.override.delete(name); else S.override.set(name, model);
  save(); return true;
}
async function modelMenu(pane, anchor) {
  const b = bot(PANE[pane].bot()); if (!b) return;
  // Read now, so an edited ~/.agent/models shows without a restart.
  let list = [], error = null;
  try { list = await Daemon.models(); if (!list.length) error = 'Settings lists your providers\' models'; } catch (e) { error = String(e?.message ?? e); }
  // The pane may show another bot, or this name another identity, by the time the list is read.
  if (PANE[pane].bot() !== b.name || bot(b.name) !== b) return;
  showMenu(modelMenuItems(b, list, error), anchor);
}

// ---------- actions ----------
// `to` is the bot the text was typed for, which is the one it goes to even if the pane has since
// been pointed at another.
async function submit(text, pane = 'main', to = PANE[pane].bot()) {
  if (pane === 'main' && text.startsWith('/new ')) {
    const [name, model] = text.slice(5).trim().split(/\s+/);
    if (!name) throw new Error('name_required');
    const m = model || S.config?.model; if (!m) throw new Error('model_required: /new NAME PROVIDER/MODEL');
    // Composed now, so an AGENTS.md edited since the window opened reaches this bot. One that
    // cannot be composed rejects here and nothing is created, as with the CLI's --agents.
    const policy = await Daemon.policy();
    const session = S.session;
    const record = await Daemon.request('create', { bot: name, workspace: S.config.workspace, model: m, instructions: policy.instructions, compaction_instructions: policy.compaction_instructions, tools: S.config.tools });
    await enqueue(() => { if (S.session === session) seat(record, session); });
    await openOnly(name); toast(`created ${name} · ${policy.note}`); return;
  }
  if (text === '/help' || text === '?') { showHelp(pane); return; }
  const b = bot(to); if (!b) throw new Error('no bot selected; /new NAME creates one');
  // An event can seat a bot before its snapshot identity arrives. Never send an unpinned name.
  if (b.id == null) throw new Error('bot_identity_pending: wait for attachment to finish');
  const mode = sendMode(b), model = S.override.get(b.name), delivery = mode === 'send' ? 'reject' : mode;
  if (mode === 'side') { await sideChat(b.name, text); return; }
  // A steer joins the running turn only on that turn's model and folder, so it names neither.
  // It also names the turn on screen, so a turn that ended meanwhile refuses it as stale_turn
  // rather than the message landing in whatever turn runs next. A bot keeps its folder, so a
  // message names one only for a bot that has none.
  const where = delivery === 'steer' ? (b.runningTurn != null ? { expected_turn: b.runningTurn } : {}) : { ...home(b), ...(model && model !== b.model ? { model } : {}) };
  // The identity on screen, so a name that changed hands in between is refused rather than handed the prompt.
  try { await Daemon.request('submit', { bot: b.name, bot_id: b.id, request_id: `app-${crypto.randomUUID()}`, prompt: text, delivery, ...where }); }
  catch (e) { if (delivery === 'steer' && /stale_turn/.test(String(e?.message ?? e))) throw new Error('that turn ended; not steered'); throw e; }
}
async function interrupt(name = S.selected) { const b = bot(name); if (!b || b.runningTurn === null) return; try { await Daemon.request('interrupt', { bot: b.name, turn: b.runningTurn }); } catch (e) { toast(`interrupt: ${e?.message ?? e}`); } }
// `NAME-fork`, then `NAME-fork-2` on (or `-side`), with NAME cut whole characters short so the
// daemon's 128-byte name limit holds.
const NAME_BYTES = 128;
function forkName(name, k, kind = 'fork') {
  const suffix = k > 1 ? `-${kind}-${k}` : `-${kind}`;
  let base = '', bytes = 0;
  for (const c of name) {
    const n = c.codePointAt(0) < 0x80 ? 1 : c.codePointAt(0) < 0x800 ? 2 : c.codePointAt(0) < 0x10000 ? 3 : 4;
    if (bytes + n + suffix.length > NAME_BYTES) break;
    base += c; bytes += n;
  }
  return base + suffix;
}
// A fork is an exact copy of a bot at rest, next to it in the tree, opened beside.
async function fork(name) {
  const b = bot(name); if (!b) return;
  if (b.id == null) throw new Error('bot_identity_pending: wait for attachment to finish');
  if (isActive(b.status)) throw new Error('bot_busy: a running bot forks once its turn ends');
  let copy = forkName(name, 1); for (let k = 2; S.bots.has(copy); k++) copy = forkName(name, k);
  // A task known only by its project prefix sits under the coordinator, and so does its fork.
  // A root bot's fork is a root too.
  const lead = b.project ? bot(b.project + LEAD) : null;
  const parent = creatorOf(b) ?? (lead && lead !== b && lead.id != null ? lead : null), session = S.session;
  const record = await Daemon.request('fork', { source: name, bot: copy, ...(parent ? { created_by: parent.name, created_by_id: parent.id } : {}) });
  await enqueue(() => { if (S.session === session) seat(record, session); });
  S.shapeGen += 1;
  await openBeside(copy);
}
// A side chat is a fork of a bot, running or not, from its newest finished round, nested under it
// and opened beside; the source is untouched. It has its source's tools and works in its source's
// folder, beside it. The first message, if any, goes to the side chat.
async function sideChat(name, text = '') {
  const b = bot(name); if (!b) return;
  if (b.id == null) throw new Error('bot_identity_pending: wait for attachment to finish');
  let copy = forkName(name, 1, 'side'); for (let k = 2; S.bots.has(copy); k++) copy = forkName(name, k, 'side');
  const session = S.session;
  const record = await Daemon.request('fork', { source: name, bot: copy, created_by: name, created_by_id: b.id });
  await enqueue(() => { if (S.session === session) seat(record, session); });
  S.shapeGen += 1;
  // The first message goes before the pane loads any history, so the turn starts at once.
  let failed = null;
  if (text) {
    try { await Daemon.request('submit', { bot: copy, bot_id: record.id, request_id: `app-${crypto.randomUUID()}`, prompt: text, delivery: 'reject', ...home(record) }); }
    catch (err) { failed = err instanceof Error ? err : new Error(String(err)); }
  }
  if (S.ui.side !== copy) await openBeside(copy);
  if (!failed) return;
  // The side chat exists, so an unsent first message waits in its composer, not its source's.
  const pane = Object.values(PANE).find((p) => p.bot() === copy), input = pane && $(pane.input);
  if (input && !input.value) { input.value = text; grow(input); }
  throw Object.assign(failed, { kept: true });
}
async function remove(name) { await Daemon.request('delete', { bot: name }); }
// A project in a folder: the folder's `.agents/project.toml` names it, or the folder's own name does,
// and its coordinator is `<name>.lead` working there. An existing coordinator is opened, not made
// twice, unless it works in another folder. The file is written only once the daemon has accepted
// the coordinator, so a model it refuses is never saved; a folder whose coordinator exists gets
// the file it lacks, with that coordinator's model, so a failed write retries.
// The app's own opinion of how a coordinator works is its `coordinator` profile: the folder's
// `.agents/agents/coordinator.md`, the user's, or the one the app ships (app/agents/coordinator.md).
async function createProject(dir, picked = null) {
  const info = await Daemon.project(dir);
  const existing = bot(info.coordinator);
  if (existing) {
    if (existing.workspace !== info.dir) throw new Error(`${info.coordinator} already belongs to ${existing.workspace ?? 'another folder'}`);
    if (!info.file) await Daemon.writeProject({ dir: info.dir, name: info.name, model: existing.model });
    await openOnly(info.coordinator); return;
  }
  const policy = await Daemon.policy(info.dir, 'coordinator');
  // The model picked when the project was made, else the folder's, its profile's, or the app's --model.
  const model = picked || info.model || policy.model || S.config?.model;
  if (!model) throw new Error('model_required: choose a model');
  if (picked) try { localStorage.setItem('agent:model', picked); } catch (_) {}
  const session = S.session;
  const record = await Daemon.request('create', { bot: info.coordinator, workspace: info.dir, model, instructions: policy.instructions, compaction_instructions: policy.compaction_instructions, tools: policy.tools ?? S.config.tools });
  await enqueue(() => { if (S.session === session) seat(record, session); });
  if (!info.file) await Daemon.writeProject({ dir: info.dir, name: info.name, model });
  await openOnly(info.coordinator); toast(`project ${info.name} · ${policy.note}`);
}
function detach() { save(); Daemon.close(); }

// ---------- setup ----------
// What a daemon needs before anything runs: providers, and what each needs to sign in. The app keeps
// them in `~/.agent/env` (read back without key values) and restarts the daemon to apply them. There
// is no default model: each project or agent is given one when it is made, from any provider.
// Onboarding is this screen opened on its own when no provider is set up; Settings is the same
// screen opened from the sidebar.
const AWS = 'Signs in with your AWS CLI login for the profile (aws configure, or aws sso login), or with a Bedrock API key if you give one.';
const BEDROCK = [
  { key: 'AWS_REGION', label: 'Region', hint: 'us-east-1', required: true },
  { key: 'AWS_PROFILE', label: 'AWS profile', hint: 'default' },
  { key: 'AWS_BEARER_TOKEN_BEDROCK', label: 'Bedrock API key', hint: 'optional', secret: true },
];
// Keys a start without AGENT_PROVIDER turns into providers, as the CLI detects them.
const DETECTED = ['ANTHROPIC_API_KEY', 'OPENAI_API_KEY', 'OPENROUTER_API_KEY'];
const CATALOG = [
  { id: 'anthropic', label: 'Anthropic', fields: [{ key: 'ANTHROPIC_API_KEY', label: 'API key', secret: true, required: true }] },
  { id: 'openai', label: 'OpenAI', fields: [{ key: 'OPENAI_API_KEY', label: 'API key', secret: true, required: true }] },
  { id: 'openrouter', label: 'OpenRouter', fields: [{ key: 'OPENROUTER_API_KEY', label: 'API key', secret: true, required: true }] },
  { id: 'chatgpt', label: 'ChatGPT plan', about: 'Uses the ChatGPT sign-in Codex saved on this computer (codex login).', fields: [] },
  // Bedrock serves Claude over Anthropic's API and every other model over OpenAI's, each a provider of
  // its own to the daemon; to the user it is one provider, connected and removed as one.
  { id: 'bedrock', label: 'Amazon Bedrock', about: AWS, fields: BEDROCK, parts: [['bedrock', 'anthropic', 'anthropic'], ['bedrock-openai', 'responses', 'openai']] },
];
const specName = (spec) => String(spec).split('=')[0];
// The daemon providers an entry runs, and the entry a daemon provider belongs to.
const partsOf = (c) => c.parts ? c.parts.map(([name]) => name) : [c.id];
const catalogOf = (name) => CATALOG.find((c) => partsOf(c).includes(name));
// The keys a spec signs with: its catalog entry's, or the one a `NAME=FAMILY,URL,KEY` spec names.
const keysOf = (spec) => [
  ...(catalogOf(specName(spec))?.fields ?? []).filter((f) => f.secret).map((f) => f.key),
  ...String(spec).split('=').slice(1).join('=').split(',').slice(2, 3).filter(Boolean),
];
const providerLabel = (name) => catalogOf(name)?.label ?? name;
// A model picker: every listed model under its provider's name, the last one picked chosen.
function lastModel() { try { return localStorage.getItem('agent:model'); } catch (_) { return null; } }
function modelSelectHTML(id, list) {
  const pick = [lastModel(), S.config?.model].find((m) => m && list.some((x) => x.id === m)) ?? '';
  const groups = new Map(); for (const m of list) { const label = providerLabel(providerOf(m.id)); if (!groups.has(label)) groups.set(label, []); groups.get(label).push(m); }
  const options = [...groups].map(([label, ms]) => `<optgroup label="${esc(label)}">${ms.map((m) => `<option value="${esc(m.id)}"${m.id === pick ? ' selected' : ''}>${esc(m.id.slice(providerOf(m.id).length + 1))}${m.note ? ` · ${esc(m.note)}` : ''}</option>`).join('')}</optgroup>`).join('');
  return `<select id="${id}" aria-label="Model">${pick ? '' : '<option value="" selected disabled>Choose a model</option>'}${options}</select>`;
}
// An entry's `--provider` specs. Bedrock with an API key names each endpoint so the key can be named
// after it; without one it signs with the AWS CLI's credentials in the region.
function providerSpecs(id, values) {
  const c = catalogOf(id);
  if (!c?.parts) return [id];
  return c.parts.map(([name, family, path]) => values.AWS_BEARER_TOKEN_BEDROCK ? `${name}=${family},https://bedrock-mantle.${values.AWS_REGION}.api.aws/${path}/v1,AWS_BEARER_TOKEN_BEDROCK` : name);
}
// One screen, one state: `settings` as the app would start a daemon with, each provider's answer
// (`checking`, a model count, or an error), and the list models are picked from.
function setupState() { return S.setup ??= { open: false, settings: null, status: {}, list: [], adding: null, busy: null, error: null, listError: null }; }
const hasProject = () => [...S.bots.keys()].some((n) => n.endsWith(LEAD));
// A window with nothing in it opens setup when no provider is set up.
async function offerSetup() {
  // No agents yet: setup connects a provider or, with one already, opens the first project.
  try { await openSetup(); } catch (_) {}
}
async function openSetup() {
  S.setupSeen = true;
  const st = setupState();
  st.open = true; st.error = null;
  $('setupwrap').classList.add('on'); renderSetup();
  try { st.settings = await Daemon.settings(); } catch (e) { st.error = String(e?.message ?? e); }
  if (!st.settings?.providers?.length) st.adding = st.adding ?? '';
  renderSetup();
  await Promise.all([checkProviders(), readList()]);
}
function closeSetup() {
  const st = S.setup; if (!st || st.busy) return;
  st.open = false; st.adding = null; $('setupwrap').classList.remove('on'); render(); focusInput('main');
}
// Each provider's own answer, read without writing anything: the daemon keeps listings a while.
async function checkProviders() {
  const st = setupState(); const names = (st.settings?.providers ?? []).map(specName);
  if (!S.attached || !names.length) { st.status = {}; renderSetup(); return; }
  st.status = Object.fromEntries(names.map((n) => [n, 'checking'])); renderSetup();
  try { const { providers } = await Daemon.request('provider_models', {}); st.status = Object.fromEntries(names.map((n) => [n, answerOf(providers?.[n])])); }
  catch (e) { st.status = Object.fromEntries(names.map((n) => [n, { error: String(e?.message ?? e) }])); }
  renderSetup();
}
const answerOf = (listed) => !listed ? { error: 'not running; restart to apply' } : Array.isArray(listed.models) ? { models: listed.models.length } : { error: listed.error ?? 'no listing', detail: listed.detail ?? null };
async function readList() {
  const st = setupState();
  try { st.list = await Daemon.models(); st.listError = null; } catch (e) { st.list = []; st.listError = String(e?.message ?? e); }
  renderSetup();
}
// Ask the providers again and write the list new agents pick from. A provider that fails keeps what
// it listed last; nothing usable leaves the old list as it was.
async function refreshModels() {
  const st = setupState();
  const names = (st.settings?.providers ?? []).map(specName);
  // No provider, no daemon to ask: the list offers nothing until one is connected.
  if (!names.length) { st.status = {}; st.list = []; st.listError = null; renderSetup(); return; }
  st.status = Object.fromEntries(names.map((n) => [n, 'checking'])); st.busy = 'Asking your providers for their models…'; renderSetup();
  let refused = null;
  try {
    const found = await Daemon.discoverModels();
    st.status = Object.fromEntries(names.map((n) => [n, found.providers?.[n] ?? answerOf(null)]));
    if (!found.written) refused = found.error;
  } catch (e) { st.error = String(e?.message ?? e); }
  finally { st.busy = null; }
  await readList();
  // Why nothing was written outlasts reading the old list back.
  if (refused) { st.listError = refused; renderSetup(); }
}
// Apply saved settings: the daemon restarts (running turns stop) and the window attaches again.
async function restartDaemon() {
  const st = setupState();
  st.busy = 'Restarting the daemon…'; renderSetup();
  try {
    await Daemon.restartDaemon();
    // With every provider removed, a daemon that will not start for lack of one is the expected end.
    if (!(await attach()) && !(await attach()) && !(/^no_provider/.test(S.lastReason ?? '') && !st.settings?.providers?.length)) throw new Error(S.lastReason ?? 'daemon_unavailable');
  } finally { st.busy = null; }
}
async function connectProvider(id, values) {
  const st = setupState(); const c = catalogOf(id); if (!c) return;
  // A key already set, saved here or exported by the shell, answers for an empty field.
  for (const f of c.fields) if (f.required && !values[f.key] && !(f.secret && st.settings?.keys?.includes(f.key))) throw new Error(`${f.label} is required`);
  const specs = (st.settings?.providers ?? []).filter((s) => catalogOf(specName(s)) !== c);
  // A saved Bedrock key stays in use when its field is left empty.
  const keyed = c.parts && !values.AWS_BEARER_TOKEN_BEDROCK && st.settings?.keys?.includes('AWS_BEARER_TOKEN_BEDROCK') ? { ...values, AWS_BEARER_TOKEN_BEDROCK: 'saved' } : values;
  const changes = { AGENT_PROVIDER: [...specs, ...providerSpecs(c.id, keyed)].join(' ') };
  // A key left empty keeps the one saved; other fields say what they say.
  for (const f of c.fields) if (!f.secret || values[f.key]) changes[f.key] = values[f.key] || null;
  await applySettings(changes);
  st.adding = null;
  await refreshModels();
}
async function removeProvider(name) {
  const st = setupState(); const c = catalogOf(name);
  const gone = c ? partsOf(c) : [name];
  const specs = (st.settings?.providers ?? []).filter((s) => !gone.includes(specName(s)));
  const changes = { AGENT_PROVIDER: specs.join(' ') || null };
  // Its key goes too, unless another provider still uses it; the region and profile stay.
  const used = specs.flatMap(keysOf);
  for (const f of c?.fields ?? []) if (f.secret && !used.includes(f.key)) changes[f.key] = null;
  // With no provider named, a start detects one from any key the shell exports; an empty key hides it.
  if (!specs.length) for (const key of DETECTED) if (key in changes || st.settings?.keys?.includes(key)) changes[key] = '';
  await applySettings(changes);
  await refreshModels();
}
async function applySettings(changes) {
  const st = setupState();
  st.busy = 'Saving…'; st.error = null; renderSetup();
  try { await Daemon.saveSettings(changes); st.settings = await Daemon.settings(); } finally { st.busy = null; }
  await restartDaemon();
}
function setupHTML() {
  const st = setupState(), set = st.settings, specs = set?.providers ?? [];
  const busy = st.busy ? ' disabled' : '';
  // One row per entry: an entry of several daemon providers counts their models together and names
  // whichever of them failed.
  const statusHTML = (names) => {
    const all = names.map((n) => st.status[n]);
    if (all.includes('checking')) return '<span class="st dim">checking…</span>';
    if (all.some((s) => !s)) return `<span class="st dim">${S.attached ? '' : 'not running'}</span>`;
    const models = all.reduce((sum, s) => sum + (s.models ?? 0), 0);
    if (all.every((s) => s.error)) return '<span class="st bad">✘ not ready</span>';
    return `<span class="st ok">✓ ${models} model${models === 1 ? '' : 's'}</span>`;
  };
  const errorsHTML = (names) => names.filter((n) => st.status[n]?.error).map((n) => { const s = st.status[n]; return `<div class="perr">${names.length > 1 ? `${esc(n)}: ` : ''}${esc(s.error)}${s.detail ? `: ${esc(String(s.detail).slice(0, 300))}` : ''}</div>`; }).join('');
  const entries = []; for (const spec of specs) { const n = specName(spec), c = catalogOf(n), key = c?.id ?? n; if (!entries.some((e) => e.key === key)) entries.push({ key, label: c?.label ?? n, names: specs.map(specName).filter((m) => (catalogOf(m)?.id ?? m) === key) }); }
  const rows = entries.map(({ key, label, names }) => { const failed = names.some((n) => st.status[n]?.error); return `<div class="prow"><span class="pn">${esc(label)}</span>${statusHTML(names)}<span class="acts">${failed ? `<button type="button" class="sbtn" data-act="setup-retry"${busy}>Retry</button>` : ''}<button type="button" class="sbtn${st.confirm === key ? ' danger' : ''}" data-act="setup-remove" data-v="${esc(key)}"${busy}>${st.confirm === key ? 'Remove anyway' : 'Remove'}</button></span>${errorsHTML(names)}${st.confirm === key ? '<div class="perr warn">Agents are working. Removing restarts the daemon, which stops them.</div>' : ''}</div>`; }).join('');
  let add = '';
  if (st.adding === null) add = `<button type="button" class="sbtn" data-act="setup-add"${busy}>＋ Add a provider</button>`;
  else if (st.adding === '') add = `<div class="choices">${CATALOG.filter((c) => !specs.some((s) => catalogOf(specName(s)) === c)).map((c) => `<button type="button" class="choice" data-act="setup-pick" data-v="${c.id}"${busy}>${esc(c.label)}</button>`).join('')}</div>${specs.length ? `<button type="button" class="sbtn" data-act="setup-cancel">Cancel</button>` : ''}`;
  else {
    const c = catalogOf(st.adding);
    const value = (f) => f.key === 'AWS_REGION' ? set?.region ?? '' : f.key === 'AWS_PROFILE' ? set?.profile ?? '' : '';
    const fields = c.fields.map((f) => `<label><span>${esc(f.label)}</span><input name="${f.key}" type="${f.secret ? 'password' : 'text'}" autocomplete="off" spellcheck="false" value="${esc(value(f))}" placeholder="${esc(f.secret && set?.keys?.includes(f.key) ? 'saved; type to replace' : f.hint ?? '')}"></label>`).join('');
    const working = anyActive() ? `<p class="warn">Agents are working. Connecting restarts the daemon, which stops them.</p>` : '';
    add = `<form class="pform" id="setupform"><b>${esc(c.label)}</b>${c.about ? `<p>${esc(c.about)}</p>` : ''}${fields}${working}<div class="row"><button type="submit" class="sbtn primary"${busy}>Connect</button><button type="button" class="sbtn" data-act="setup-cancel"${busy}>Cancel</button></div></form>`;
  }
  const ready = Object.values(st.status).some((s) => s?.models > 0) || st.list.length > 0;
  const refresh = specs.length && S.attached ? `<button type="button" class="sbtn" data-act="setup-refresh"${busy}>Refresh models</button>` : '';
  const listed = st.listError ? `<p class="bad">${esc(st.listError)}</p>` : '';
  const projects = hasProject();
  const project = projects ? '' : ready && specs.length && st.list.length
    ? `<form id="setupproj"><label><span>Folder</span><input id="setupdir" autocomplete="off" spellcheck="false" value="${esc(S.config?.workspace ?? '')}"></label><label><span>Model</span>${modelSelectHTML('setupmodel', st.list)}</label><div class="row"><button type="submit" class="sbtn primary"${S.attached ? '' : ' disabled'}${busy}>Create project</button></div><p class="dim">The project's lead runs on this model; every agent you start can use another.</p></form>`
    : `<p class="dim">${specs.length ? 'No models listed yet: see the providers above, then Refresh models.' : 'Connect a provider first.'}</p>`;
  const step = (n, title, done, body) => body ? `<section class="${done ? 'done' : ''}"><h3><span class="num">${done ? '✓' : n}</span>${title}</h3>${body}</section>` : '';
  return `<div class="shead"><b>${ready && projects ? 'Settings' : 'Set up Agent'}</b><button type="button" class="ibtn" data-act="setup-close" title="Close" aria-label="Close"${busy}>✕</button></div>`
    + step(1, 'Providers', ready, `${rows}<div class="row">${add}${st.adding === null ? refresh : ''}</div>${listed}`)
    + step(2, 'First project', projects, project)
    + (st.busy ? `<p class="busy">${esc(st.busy)}</p>` : '') + (st.error ? `<p class="bad">${esc(st.error)}</p>` : '');
}
// Answers arrive while someone types a key: what the fields hold, and where the caret is, survive.
function renderSetup() {
  if (!S.setup?.open) return;
  const box = $('setup'), kept = new Map();
  for (const el of box.querySelectorAll('input')) kept.set(el.name || el.id, el.value);
  const focused = box.contains?.(document.activeElement) ? document.activeElement.name || document.activeElement.id : null;
  box.innerHTML = setupHTML();
  for (const el of box.querySelectorAll('input')) { const v = kept.get(el.name || el.id); if (v !== undefined) el.value = v; if (focused && (el.name || el.id) === focused) el.focus(); }
}

// ---------- opening threads ----------
// From the sidebar or the finder a thread takes the whole window, with nothing beside it.
async function openOnly(name) {
  if (!S.bots.has(name)) return;
  S.selected = name; S.autoSelect = false; S.ui.side = null;
  const p = bot(name).project; if (p && S.ui.folded.delete(p)) S.shapeGen += 1;
  await enqueue(loadVisible); render(); save();
}
// A task card opens its bot beside the thread; clicking it again closes it.
async function openBeside(name) {
  if (!S.bots.has(name) || name === S.selected) return;
  S.ui.side = S.ui.side === name ? null : name;
  await enqueue(loadVisible); render(); save();
  focusInput(S.ui.side ? 'side' : 'main');
}
// Ctrl-P puts the next peer beside, with its own draft.
async function nextBeside() {
  const ps = peers().filter((who) => who !== S.selected); if (!ps.length) return;
  const next = ps[(ps.indexOf(S.ui.side) + 1) % ps.length]; if (next === S.ui.side) return;
  S.ui.side = next;
  await enqueue(loadVisible); render(); save(); focusInput('side');
}
// Drafts travel with their bots.
function swap() {
  if (!S.ui.side) return;
  [S.selected, S.ui.side] = [S.ui.side, S.selected];
  const p = bot(S.selected).project; if (p && S.ui.folded.delete(p)) S.shapeGen += 1;
  render(); save(); focusInput('main');
}
function closeSide() { if (!S.ui.side) return; S.ui.side = null; render(); save(); focusInput('main'); }
function focusInput(pane) { const el = $(PANE[pane].input); if (el) setTimeout(() => el.focus({ preventScroll: true }), 0); }
function showNewProject(on) {
  $('projform').hidden = !on; $('newproj').hidden = on;
  if (!on) return;
  $('projdir').value = S.config?.workspace ?? ''; $('projdir').focus();
  // The lead's model, from every provider's list, read now so a refreshed list shows.
  $('projmodel').innerHTML = '';
  Daemon.models().then((list) => { if (!$('projform').hidden) $('projmodel').innerHTML = list.length ? modelSelectHTML('projsel', list) : '<span class="dim">no models listed: see Settings</span>'; }, (e) => { $('projmodel').textContent = String(e?.message ?? e); });
}

// ---------- input ----------
function grow(el) { if (!el.style) return; el.style.height = 'auto'; el.style.height = `${Math.min(160, el.scrollHeight)}px`; }
for (const [pane, ids] of Object.entries(PANE)) {
  $(ids.form).addEventListener('submit', async (e) => {
    e.preventDefault(); const input = $(ids.input); const v = input.value.trim(); if (!v) return; input.value = ''; grow(input);
    // The text goes to the bot it was typed for; a failed send comes back to that bot's draft,
    // in whichever pane shows it now, never over new typing.
    const who = input.dataset.for || PANE[pane].bot();
    try { await submit(v, pane, who); } catch (err) {
      toast(String(err?.message ?? err));
      if (err?.kept) return;
      const shown = Object.values(PANE).map((p) => $(p.input)).find((el) => el.dataset.for === who);
      if (shown) { if (!shown.value) { shown.value = v; grow(shown); } } else if (who && !S.drafts.get(who)) S.drafts.set(who, v);
    }
  });
  $(ids.input).addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); $(ids.form).requestSubmit(); } });
  $(ids.input).addEventListener('input', () => { const input = $(ids.input); grow(input); if (pane === 'main' && input.value === '?') { input.value = ''; showHelp(); } });
}
$('projform').addEventListener('submit', async (e) => {
  e.preventDefault(); const dir = $('projdir').value.trim(); if (!dir) return;
  try { await createProject(dir, $('projsel')?.value || null); showNewProject(false); focusInput('main'); } catch (err) { toast(String(err?.message ?? err), 5000); }
});
$('projform').addEventListener('keydown', (e) => { if (e.key === 'Enter' && e.target.id === 'projsel') { e.preventDefault(); $('projform').requestSubmit(); } else if (e.key === 'Escape') { showNewProject(false); focusInput('main'); e.preventDefault(); e.stopPropagation(); } });
$('pickerq').addEventListener('input', renderPicker);
$('pickerq').addEventListener('keydown', async (e) => {
  const rows = pickerRows();
  if (e.key === 'Escape') { closePicker(); e.preventDefault(); }
  else if (e.key === 'ArrowDown' || (e.ctrlKey && e.key === 'n')) { S.ui.pickerSel = Math.min(rows.length - 1, S.ui.pickerSel + 1); renderPicker(); e.preventDefault(); }
  else if (e.key === 'ArrowUp' || (e.ctrlKey && e.key === 'p')) { S.ui.pickerSel = Math.max(0, S.ui.pickerSel - 1); renderPicker(); e.preventDefault(); }
  else if (e.key === 'Enter') { const r = rows[S.ui.pickerSel]; closePicker('main'); if (r) await openOnly(r.b.name); e.preventDefault(); }
});
$('pickerlist').addEventListener('click', async (e) => { const r = e.target.closest('[data-pick]'); if (r) { closePicker('main'); await openOnly(r.dataset.pick); } });
const inputIds = new Set(['input', 'sideinput', 'projdir', 'pickerq']);
document.addEventListener('keydown', async (e) => {
  if (S.ui.help) { hideHelp(); e.preventDefault(); return; }
  if (S.setup?.open) { if (e.key === 'Escape') { closeSetup(); e.preventDefault(); } return; }
  if ((e.ctrlKey || e.metaKey) && e.key === ',') { await openSetup(); e.preventDefault(); return; }
  // The finder and the folder field handle their own keys; Escape there must not stop a turn.
  if (S.ui.picker || e.target.id === 'projdir' || e.target.id === 'projsel' || e.target.id === 'pickerq') return;
  const k = e.key, ctrl = e.ctrlKey || e.metaKey;
  if (S.ui.menu) { if (k === 'Escape') { closeMenu(); e.preventDefault(); } return; }
  if (ctrl && k === 'k') { openPicker(); e.preventDefault(); return; }
  if (ctrl && k === 'b') { S.ui.rail = !S.ui.rail; render(); save(); e.preventDefault(); return; }
  if (ctrl && k === 'd') { detach(); e.preventDefault(); return; }
  if (ctrl && k === 'o') { S.ui.steps = !S.ui.steps; render(); save(); e.preventDefault(); return; }
  if (ctrl && k === 'p') { await nextBeside(); e.preventDefault(); return; }
  if (k === 'Escape') { if (S.ui.side) closeSide(); else await interrupt(); e.preventDefault(); return; }
  const empty = e.target.id === 'input' && $('input').value === '';
  if (empty && (k === 'ArrowUp' || k === 'ArrowDown')) { const names = tree().filter((n) => n.b).map((n) => n.b.name); let i = names.indexOf(S.selected); if (i >= 0) { i = (i + (k === 'ArrowDown' ? 1 : names.length - 1)) % names.length; await openOnly(names[i]); } e.preventDefault(); return; }
  if (!inputIds.has(e.target.id) && k.length === 1 && !ctrl && !e.altKey) $('input').focus();
});
function failed(err) {
  const text = String(err?.message ?? err);
  if (S.setup?.open) { S.setup.error = text; renderSetup(); } else toast(text, 4000);
}
$('setup').addEventListener('submit', async (e) => {
  e.preventDefault();
  const form = e.target;
  try {
    if (form.id === 'setupform') await connectProvider(S.setup.adding, Object.fromEntries([...form.querySelectorAll('input')].map((el) => [el.name, el.value.trim()])));
    else if (form.id === 'setupproj') { const dir = $('setupdir').value.trim(), model = $('setupmodel').value; if (dir) { await createProject(dir, model); closeSetup(); } }
  } catch (err) { failed(err); }
});
async function act(el) {
  const a = el.dataset.act, who = el.dataset.who, v = el.dataset.v, pane = el.dataset.pane, rect = el.getBoundingClientRect?.();
  switch (a) {
    case 'more': showMenu(botMenuItems(who), { rect }, who); return;
    case 'model': await modelMenu(pane, { rect, up: true }); return;
    case 'sendmenu': showMenu(sendMenuItems(pane), { rect, up: true }); return;
    case 'set-model': setModel(who, v); render(); focusInput(who === S.ui.side ? 'side' : 'main'); return;
    case 'set-send': setSend(v); render(); focusInput(pane); return;
    case 'side-chat': await sideChat(who); return;
    case 'stop': await interrupt(who); return;
    case 'stop-pane': await interrupt(PANE[pane].bot()); return;
    case 'fork': await fork(who); return;
    case 'delete': showMenu([{ act: 'delete-yes', who, label: `Delete ${shortName(bot(who))}`, danger: true }, { act: 'close-menu', label: 'Cancel' }], menuAnchor ?? { rect }); return;
    case 'delete-yes': await remove(who); return;
    case 'steps': S.ui.steps = !S.ui.steps; render(); save(); return;
    case 'fold': if (!S.ui.folded.delete(v)) S.ui.folded.add(v); S.shapeGen += 1; render(); save(); return;
    case 'open': await openOnly(who); return;
    case 'swap': swap(); return;
    case 'close-side': closeSide(); return;
    case 'new-project': showNewProject(true); return;
    case 'settings': await openSetup(); return;
    case 'setup-close': closeSetup(); return;
    case 'setup-add': setupState().adding = ''; renderSetup(); return;
    case 'setup-pick': setupState().adding = v; renderSetup(); $('setup').querySelector('#setupform input')?.focus(); return;
    case 'setup-cancel': setupState().adding = null; renderSetup(); return;
    // Removing restarts the daemon; with agents working, the first press says so and the second removes.
    case 'setup-remove': { const st = setupState(); if (anyActive() && st.confirm !== v) { st.confirm = v; renderSetup(); return; } st.confirm = null; await removeProvider(v); return; }
    case 'setup-retry': case 'setup-refresh': await refreshModels(); return;
    default: return;
  }
}
document.addEventListener('click', async (e) => {
  if (S.ui.help) { hideHelp(); return; }
  if (e.target.closest('#pickerwrap') && !e.target.closest('.picker')) { closePicker(); return; }
  const button = e.target.closest('[data-act]');
  closeMenu();
  if (button) { if (!button.disabled) { try { await act(button); } catch (err) { failed(err); } } return; }
  if (e.target.closest('#setupwrap')) return;
  if (e.target.closest('#menu')) return;
  const step = e.target.closest('[data-out], [data-run]'), task = e.target.closest('[data-task]'), row = e.target.closest('[data-bot]');
  if (step) toggleStep(step);
  // Opening a task beside puts the keyboard where openBeside chose.
  else if (task) { await openBeside(task.dataset.task); return; }
  else if (row) await openOnly(row.dataset.bot);
  // Clicks return the keyboard to the pane's composer, unless they selected text to copy.
  if (!e.target.closest('input, textarea, form') && window.getSelection?.()?.isCollapsed !== false) focusInput(e.target.closest('.pane.side') ? 'side' : 'main');
});
document.addEventListener('contextmenu', (e) => {
  const t = e.target.closest('[data-bot], [data-task]'); if (!t) return;
  const who = t.dataset.bot ?? t.dataset.task;
  e.preventDefault(); closeMenu(); showMenu(botMenuItems(who), { x: e.clientX, y: e.clientY }, who);
});

// ---------- boot ----------
render();
attach().then(() => $('input').focus());
})();
