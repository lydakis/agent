// The Thread client. State is built from the daemon's event stream exactly
// as the terminal client builds it; rendering is the prototype's, verbatim.
(() => {
'use strict';
const $ = (id) => document.getElementById(id);
const GLYPH = { running: '●', waiting: '◐', paced: '◔', failed: '✘', idle: '○', done: '✔', queued: '◌', ready: '◌', interrupted: '✘' };
const LABEL = { running: 'working', waiting: 'waiting', paced: 'rate limited', failed: 'failed', idle: 'idle', done: 'done', queued: 'queued', ready: 'queued', interrupted: 'interrupted' };
const LAZY_ITEMS = 400;
// Decoded items kept around the reader's end of a transcript; bodies beyond it fold back into their
// nodes and a scroll toward them loads them again.
const WINDOW = 3 * LAZY_ITEMS;
const PEER_WINDOW = 300;
const DECODE_BYTES = 8 * 1024 * 1024;

const S = {
  bots: new Map(), transcripts: new Map(), selected: '', cursor: 0, live: false, attached: false, autoSelect: true,
  // Whether the workspace is the home a host named, not a folder the window was given.
  homeWorkspace: false,
  // The store identity the attached daemon announced; what the window remembers is saved under it.
  store: null,
  // Bumped whenever a bot is added, removed or changes status (botsGen), and when one is added or
  // removed (shapeGen), so the activity check and the rail's tree rebuild once per change instead of
  // scanning the fleet on every event.
  botsGen: 0, shapeGen: 0, deleted: new Set(),
  config: null, ui: { rail: true, side: null, picker: false, pickerSel: 0, help: false, steps: false, toast: null, menu: false, folded: new Set() },
  // How Send reaches a working bot, the last pick from its menu (sticky across windows), and the
  // model each bot's next turns run on when it differs from the one it was created with.
  send: loadSend(), override: new Map(), effort: new Map(),
  // Each provider's model family, from the bot records that name both; a turn may run on any
  // provider of its bot's family.
  families: new Map(),
  // Swarms, from their folders in ~/.agent/swarms, and the swarm each agent belongs to.
  swarms: new Map(), memberOf: new Map(),
  // Who asked for each turn another bot asked for, until it ends, and what each coordinator has yet to
  // hear about its tasks (see `wake`), and turns that ended live before the snapshot said who made their bot.
  turnFrom: new Map(), turnOrigin: new Map(), wakes: new Map(), heldNews: [],
  // Bots whose finished turn the person has not looked at yet (see `shownStatus`), and turns another
  // bot asked for that the person steered into, whose end is theirs to see too.
  unseen: new Set(), wanted: new Set(),
  // Unsent text for each bot not on screen. A composer's text is its bot's own: when a pane shows
  // another bot, the text stays behind with the one it was typed for (see `followDrafts`).
  drafts: new Map(),
};
function loadSend() { try { const v = localStorage.getItem('agent:send'); return v === 'steer' || v === 'side' ? v : 'queue'; } catch (_) { return 'queue'; } }
// What a window remembers belongs to the store it shows and its folder, not to the socket that reached
// it: two hosts, or a host and this machine, never share it. Nothing is saved before the store is known.
const sessionKey = () => (S.store ? `agent:${S.store}|${S.config?.workspace}` : null);
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
  for (const e of entries) { e.from = bare.node; e.fromCall = bare.callId; if (bare.by && e.kind === 'user') e.by = bare.by; count(t, e, 1); }
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
// A turn the person or the app asked for that finished while its bot was off screen shows as done,
// not idle, until the person looks at that bot. A failure already shows until the next turn.
const shownStatus = (b) => S.unseen.has(b.name) && b.status === 'idle' ? 'done' : b.status;
// Settings, help, the picker and the swarm sheet cover the thread. A swarm's view shows its agents.
const covered = () => S.ui.help || S.ui.picker || S.ui.sheet || S.setup?.open;
const looking = () => !covered() && document.visibilityState !== 'hidden' && document.hasFocus?.() !== false;
const onScreen = (name) => {
  if (!looking()) return false;
  const sw = swarmOfBot(name);
  // A file open beside covers the chat that was there.
  return S.selected === name || (S.ui.side === name && !S.ui.file) || (!!sw && S.selected === swarmKey(sw.name));
};
// Each row a change touches is drawn once, however many of a swarm's agents it covers.
function patchUnseen(names) {
  const rows = new Set();
  for (const name of names) { rows.add(name); const sw = swarmOfBot(name); if (sw) rows.add(swarmKey(sw.name)); }
  for (const row of rows) patchRailRow(row);
}
// Only what the panes show can become seen: the bots in them, or the selected swarm's agents.
function markSeen() {
  if (!S.unseen.size || !looking()) return;
  const sw = swarmOf(S.selected);
  const seen = [...new Set([S.selected, S.ui.file ? null : S.ui.side, ...(sw ? sw.members : [])])].filter((name) => name && S.unseen.has(name));
  if (!seen.length) return;
  for (const name of seen) S.unseen.delete(name);
  patchUnseen(seen);
  refreshLive($('log')); if (S.ui.side && !S.ui.file) refreshLive($('side'));
}
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
  learnEffort(b, record);
  learnFamily(b, record);
  learnWorkspace(b, record);
  if (record.created_by) { b.parent = record.created_by; b.parentId = record.created_by_id ?? null; }
  S.bots.set(b.name, b);
  seedHistory(record);
}
// A bot's effort level is set when it is made and kept for life; a record that does not name it says nothing.
function learnEffort(b, record) { if ('reasoning' in record) b.reasoning = record.reasoning ?? null; }
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
  // A host's folders are its own; the app does not read them yet.
  if (!ws || !Daemon.branch || S.config?.host) return;
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
  S.bots.delete(name); S.transcripts.delete(name); S.override.delete(name); S.effort.delete(name); S.unseen.delete(name);
  // A coordinator gone hears nothing more, and its queued turns never end; a task gone is no news.
  if (S.wakes.has(name)) { clearTimeout(S.wakes.get(name).timer); S.wakes.delete(name); }
  for (const w of S.wakes.values()) if (w.tasks.delete(name) && !w.tasks.size) { clearTimeout(w.timer); w.timer = null; }
  for (const map of [S.turnFrom, S.turnOrigin]) for (const key of map.keys()) if (key.startsWith(`${name}\u0000`)) map.delete(key);
  for (const key of S.wanted) if (key.startsWith(`${name}\u0000`)) S.wanted.delete(key);
  // Held news is this bot's; a later bot of the same name is another.
  S.heldNews = S.heldNews.filter(([held]) => held !== name);
  // A draft belongs to its bot, so it goes with it.
  if (S.ui.side === name) S.ui.side = null;
  if (S.ui.file?.bot === name) dropFile();
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
// Who sent a prompt that is not yours: another agent's turn, or the app on its own (`origin`), as
// the daemon keeps them with the prompt.
function senderOf(p) {
  if (p.from?.bot) return { bot: p.from.bot, turn: p.from.turn, id: p.from.id };
  return typeof p.origin === 'string' ? { app: p.origin } : null;
}
// The agent that sent it, while its name still holds the identity it had then.
const senderBot = (by) => { const b = bot(by.bot); return b && b.id != null && b.id === by.id ? b : null; };
// Who another agent is, as a message it sent names it: a coordinator by its role.
const agentName = (name, b = bot(name)) => leadProject(name) ? 'coordinator' : b ? shortName(b) : name;
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
  const swarmsOf = new Map();
  for (const sw of S.swarms.values()) { const p = projects.has(sw.project) ? sw.project : null; if (!swarmsOf.has(p)) swarmsOf.set(p, []); swarmsOf.get(p).push(sw); }
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
  // A swarm's agents, and whatever they made, are drawn in the swarm's own view, not here.
  for (const sw of S.swarms.values()) for (const m of sw.members) { const b = memberBot(sw, m); if (b && !seen.has(m)) { stack.push([b, 1, true, '', sw.project]); walk(true); } }
  const swarmRows = (list, depth) => [...(list ?? [])].sort((a, c) => a.name.localeCompare(c.name)).map((sw) => ({ swarm: sw, key: swarmKey(sw.name), depth, prefix: depth ? '├ ' : '' }));
  for (const p of [...projects.keys()].sort()) {
    const lead = projects.get(p); seen.add(lead.name); lead.project = p;
    const head = { b: lead, depth: 0, prefix: '', head: p, tasks: 0 }; out.push(head);
    // Its swarms first, one row each, then its tasks.
    const folded = !all && S.ui.folded.has(p), rows = swarmRows(swarmsOf.get(p), 1);
    if (!folded) out.push(...rows);
    const before = out.length;
    pushKids(lead.name, 1, '', p, prefixed.get(p)); head.tasks = walk(folded) + rows.length;
    if (rows.length && out.length === before) rows.at(-1).prefix = '└ ';
  }
  const loose = out.length;
  pushKids(null, 0, '', null); walk(false);
  out.push(...swarmRows(swarmsOf.get(null), 0));
  // Anything the roots do not reach is rooted where it stands: one pass, nothing hidden.
  for (const b of S.bots.values()) if (!seen.has(b.name)) { stack.push([b, 0, true, '', null]); walk(false); }
  if (projects.size && out.length > loose) out.splice(loose, 0, { label: 'bots' });
  return out;
}
// The path a read, write or edit names, whole (the summary is cut for display); none when it cannot be one.
const FILE_TOOLS = new Set(['read', 'write', 'edit']);
const toolPath = (name, parsed) => FILE_TOOLS.has(name) && typeof parsed?.path === 'string' && parsed.path && parsed.path.length <= 4096 ? parsed.path : undefined;
function callSummary(name, args) {
  let a = {}; try { a = JSON.parse(args) ?? {}; } catch (_) {}
  let s = name === 'shell' ? a.command ?? '' : ['read', 'write', 'edit'].includes(name) ? a.path ?? '' : name === 'wait' ? (Array.isArray(a.handles) ? a.handles : []).filter((h) => typeof h === 'string').map((h) => h.replace(/^turn:/, '')).join(', ') : args;
  return String(s).split('\n')[0].slice(0, 300);
}

// ---------- swarms ----------
// A swarm is the app's: its folder in ~/.agent/swarms holds its goal, its members and its board, and its
// agents are ordinary bots named `<swarm>-N`. It is one row under its project, selected by a key no bot
// name can be; its agents live in its view. Its board is read only while it is on screen, from where the
// last read ended, whenever one of its agents does something durable, so a quiet swarm costs nothing.
const SWARM = '⁂';
const swarmKey = (name) => SWARM + name;
const swarmOf = (key) => (typeof key === 'string' && key.startsWith(SWARM) ? S.swarms.get(key.slice(SWARM.length)) ?? null : null);
const isOpen = (key) => S.bots.has(key) || !!swarmOf(key);
// A member is the bot the swarm pinned: another bot later given its name is not the swarm's.
const memberBot = (sw, m) => { const b = bot(m); return b && b.id != null && b.id === sw.ids[m] ? b : null; };
const swarmOfBot = (name) => { const sw = S.swarms.get(S.memberOf.get(name)); return sw && memberBot(sw, name) ? sw : null; };
const memberShort = (sw, name) => (name.startsWith(sw.project + '.') ? name.slice(sw.project.length + 1) : name);
// `batch` defers the member index to its caller, which builds it once for all the records it learns.
function learnSwarm(record, batch = false) {
  const sw = S.swarms.get(record.swarm) ?? { lines: [], offset: null, tab: 'board', used: null, reading: null, again: false, usage: null, state: { roles: {}, streams: {}, proposals: [] }, filter: null };
  Object.assign(sw, { name: record.swarm, dir: record.dir, project: record.project, goal: record.goal, workspace: record.workspace, budget: record.budget_tokens, mix: record.mix ?? [], members: record.members ?? [], ids: record.ids ?? {}, rows: record.rows ?? {}, stopped: !!record.stopped, council: record.council ?? 0, seats: record.seats ?? [], left: record.left ?? [] });
  S.swarms.set(sw.name, sw); if (!batch) indexMembers();
  return sw;
}
function indexMembers() {
  S.memberOf = new Map(); for (const sw of S.swarms.values()) for (const m of sw.members) S.memberOf.set(m, sw.name);
  S.shapeGen += 1;
}
// A swarm's departures go one at a time, so each answer is newer than the one before it and the last
// one applied has them all. The board is read after each: a stream or a seat may have changed hands.
const leaving = new Map();
function leave(swarm, name) {
  const next = (leaving.get(swarm) ?? Promise.resolve()).then(() => Daemon.swarmLeave(swarm, name)).then(
    (r) => {
      const sw = learnSwarm(r); boardSoon(sw); if (S.selected === swarmKey(swarm)) render();
      // Who was not told it leads or sits now; the board still says it.
      if (r.missed?.length) toast(`${swarm}: not told: ${r.missed.map((m) => `${m.agent} (${m.error})`).join(', ')}`, 5000);
    },
    (e) => toast(`leave ${swarm}: ${e?.message ?? e}`));
  leaving.set(swarm, next);
  next.then(() => { if (leaving.get(swarm) === next) leaving.delete(swarm); });
  return next;
}
// Reads can overlap; only the newest one's answer is kept, so an older snapshot never removes a swarm a
// newer read found.
let swarmsRead = 0;
async function loadSwarms() {
  // Swarms live on this machine; a window on a host has none.
  if (!Daemon.swarms || S.config?.host) return;
  const read = ++swarmsRead;
  const { swarms = [], broken = [] } = await Daemon.swarms();
  if (read !== swarmsRead) return;
  const seen = new Set(swarms.map((r) => r.swarm));
  for (const name of [...S.swarms.keys()]) if (!seen.has(name)) S.swarms.delete(name);
  for (const r of swarms) learnSwarm(r, true);
  indexMembers();
  if (broken.length) toast(`not a readable swarm: ${broken[0]}`, 5000);
}
// A swarm works while any agent works, waits while any waits, and is otherwise at rest.
// At rest, it is done while an agent's finished turn is unseen, even if a later turn of it failed.
function swarmStatus(sw) {
  let out = 'idle';
  for (const m of sw.members) { const b = memberBot(sw, m), st = b?.status; if (st === 'running') return 'running'; if (st === 'waiting' || st === 'paced') out = 'waiting'; else if (out === 'idle' && b && S.unseen.has(m)) out = 'done'; }
  return out;
}
const BOARD_LINES = 500;
function readBoard(sw) {
  if (sw.reading) { sw.again = true; return sw.reading; }
  sw.reading = (async () => {
    try {
      do {
        sw.again = false;
        const from = sw.offset, r = await Daemon.swarmBoard(sw.name, from);
        // The board's tail, read afresh: it was rewritten, or grew past what is kept since the last read.
        if (r.reset) sw.lines = [];
        sw.lines.push(...(r.lines ?? []));
        // Roles, proposals and streams: what the whole board adds up to, not only the lines read.
        if (r.state) { sw.state = r.state; sw.stateGen = (sw.stateGen ?? 0) + 1; }
        if (sw.lines.length > BOARD_LINES) sw.lines.splice(0, sw.lines.length - BOARD_LINES);
        sw.offset = r.offset;
        // More to read only when this read got somewhere: a board ending in half a line waits for its
        // next event instead of being read again and again.
        if (r.more && r.offset !== from) sw.again = true;
      } while (sw.again);
    } catch (e) { toast(`board: ${e?.message ?? e}`); }
    finally { sw.reading = null; }
    if (S.selected === swarmKey(sw.name)) render();
  })();
  return sw.reading;
}
// Tokens its agents and their helpers have used, from the daemon's list: they sit together in its
// name order, a helper named after the agent that made it, so after its maker. A member that left
// still makes its helpers count, and a helper deleted since the board last looked counts with what
// the board saw it use, as the swarm's budget notices count them.
async function readUsage(sw) {
  if (sw.usage) return sw.usage;
  sw.usage = (async () => {
    // Helpers seen before stand in for their maker once it is deleted.
    const prefix = sw.name + '-', makers = new Set([...Object.values(sw.ids), ...(sw.left ?? []), ...Object.keys(sw.state?.roots ?? {}).map(Number)]), seen = new Set();
    let used = 0, after = sw.name;
    try {
      for (;;) {
        const page = await Daemon.request('bots', { after, limit: 256 }); let past = false;
        for (const r of page.bots ?? []) {
          if (r.name > prefix && !r.name.startsWith(prefix)) { past = true; break; }
          const member = sw.members.includes(r.name);
          if (member ? sw.ids[r.name] === r.id : makers.has(r.created_by_id)) { if (!member) { makers.add(r.id); seen.add(String(r.id)); } used += r.tokens_used ?? 0; }
        }
        if (past || !page.next_after) break;
        after = page.next_after;
      }
      const kept = sw.state?.helpers ?? {};
      for (const id in kept) if (!seen.has(id)) used += kept[id];
      sw.used = used + (sw.state?.gone ?? 0);
    } catch (_) { sw.used = null; }
    finally { sw.usage = null; }
    if (S.selected === swarmKey(sw.name)) render();
  })();
  return sw.usage;
}
// Usage events check allowances during work, not only after a turn has exhausted its budget.
// Coalesce across members and keep one request in flight per swarm.
const checkTimers = new Map(), checkingBudgets = new Map();
// A usage event adds what the daemon counts (input, cached included, and output). A check lists the
// swarm's bots and takes its board lock, so it waits until the tokens since the last one could move a
// member's share by a twentieth: each 50, 65 and 80% warning lands within five points of its mark.
function usageDue(sw, data) {
  sw.unchecked = (sw.unchecked ?? 0) + (data?.input_tokens ?? 0) + (data?.output_tokens ?? 0);
  return sw.unchecked >= (sw.budget ?? 0) / Math.max(1, sw.members.length) / 20;
}
function checkSoon(sw) {
  if (sw.stopped || !Daemon.swarmCheck) return;
  const inFlight = checkingBudgets.get(sw.name);
  if (inFlight) { inFlight.again = true; return; }
  if (checkTimers.has(sw.name)) return;
  // Budget and stall notices may have no later member event: read the board
  // only when the check says it appended an entry.
  const session = S.session;
  checkTimers.set(sw.name, setTimeout(async () => {
    checkTimers.delete(sw.name); if (S.session !== session || sw.stopped) return;
    const check = { again: false }; checkingBudgets.set(sw.name, check); sw.unchecked = 0;
    try { const r = await Daemon.swarmCheck(sw.name); if (S.session === session && r?.board_changed) boardSoon(sw); }
    catch (e) { Daemon.log?.(`budget check ${sw.name}: ${e?.message ?? e}`); }
    finally { checkingBudgets.delete(sw.name); if (check.again && S.session === session && !sw.stopped) checkSoon(sw); }
  }, 250));
}
// A helper's tokens count in its swarm's, so its finished turn is accounted as a member's is: its maker,
// or its maker's maker, is a member, or a member that left, known by its bot id once it is gone.
function swarmOfHelper(name) {
  let b = bot(name);
  for (let depth = 0; b && depth < 16; depth++) {
    for (const sw of S.swarms.values()) if (b.parentId != null && (sw.left?.includes(b.parentId) || sw.state?.roots?.[b.parentId] != null)) return sw;
    const maker = creatorOf(b); if (!maker) return null; const sw = swarmOfBot(maker.name); if (sw) return sw; b = maker;
  }
  return null;
}
// A swarm a coordinator started from its shell shows once its first agent takes its brief: an agent's
// first turn comes after it joined. Each name is looked for once while its bot lives, so a task named
// like an agent costs one read of the swarms, not one per event.
const BRIEFED = new Set(['accepted', 'queued']), SWARM_AGENT = /-\d+$/, looked = new Set();
let swarmsTimer = null;
let lookFor = [];
function swarmsSoon(name) {
  if (looked.has(name)) return; looked.add(name); lookFor.push(name);
  if (swarmsTimer) return;
  swarmsTimer = setTimeout(async () => {
    swarmsTimer = null; const before = new Set(S.swarms.keys()), names = lookFor; lookFor = [];
    // A read that failed looks again at these names' next turn.
    try { await loadSwarms(); } catch (e) { for (const n of names) looked.delete(n); Daemon.log?.(`swarms: ${e?.message ?? e}`); return; }
    const added = [...S.swarms.keys()].filter((n) => !before.has(n));
    if (added.length) { toast(`swarm ${added.join(', ')} started`, 4000); render(); }
  }, 200);
}
const boardTimers = new Map();
function boardSoon(sw, usage) {
  if (usage) sw.usageDue = true;
  if (boardTimers.has(sw.name)) return;
  boardTimers.set(sw.name, setTimeout(() => { boardTimers.delete(sw.name); if (S.selected !== swarmKey(sw.name)) return; readBoard(sw); if (sw.usageDue) { sw.usageDue = false; readUsage(sw); } }, 150));
}
// A swarm's mix is rows of an identity (a profile, or '' for a plain agent), a model and a share in
// percent. Its agents are dealt one at a time, each to the row furthest below its share of the agents
// so far, so any prefix of them is as close to the mix as whole agents allow: the council's seats (its
// first agents) mix too, and an added agent keeps the swarm on its shares.
function nextRow(mix, counts) {
  const t = counts.reduce((a, c) => a + c, 0) + 1;
  let best = 0; mix.forEach((row, i) => { if (row.share * t - 100 * counts[i] > mix[best].share * t - 100 * counts[best]) best = i; });
  return best;
}
function mixRows(mix, n) {
  const counts = mix.map(() => 0), rows = [];
  for (let k = 0; k < n; k++) { const i = nextRow(mix, counts); counts[i] += 1; rows.push(i); }
  return rows;
}
const mixCounts = (mix, rows) => { const counts = mix.map(() => 0); for (const r of rows) if (r in counts) counts[r] += 1; return counts; };
const modelShort = (id) => String(id ?? '').slice(providerOf(id).length + 1) || String(id ?? '');
// What a member is, where the swarm has more than one kind: its identity, and its model when they differ.
function kindOf(sw, member) {
  const row = sw.mix[sw.rows[member]]; if (!row) return '';
  const models = new Set(sw.mix.map((r) => r.model));
  return [row.identity, models.size > 1 && modelShort(row.model)].filter(Boolean).join(' · ');
}
// Starting, adding and stopping are one call each: the app's side makes the agents, has them join and
// briefs them, or ends their turns, and undoes what a failed start made. The page learns the result.
async function learnStarted(r, want) {
  const session = S.session;
  await enqueue(() => { if (S.session === session) for (const b of r.bots ?? []) seat(b, session); });
  const sw = learnSwarm(r.swarm);
  const failed = r.failed ?? [];
  if (failed.length) toast(`${want - failed.length} of ${want} agents started: ${failed[0].agent}: ${failed[0].error}`, 6000);
  return sw;
}
// The app's side names the swarm from its goal and deals its agents to the mix, as for a coordinator's
// `start`, and the sheet's counts come from the same rule.
async function createSwarm(project, { goal, n, mix, shared, budget, council = 0 }) {
  const lead = bot(project + LEAD); if (!lead?.workspace) throw new Error(`no coordinator for ${project}`);
  goal = goal.trim(); if (!goal) throw new Error('goal_required: a swarm needs a goal');
  const r = await Daemon.swarmStart({ project, folder: lead.workspace, goal, shared, mix, agents: n, budgetTokens: budget, council });
  const sw = await learnStarted(r, n);
  await openOnly(swarmKey(sw.name));
}
// An added agent comes from the row furthest below its share.
async function addAgent(sw) {
  const row = nextRow(sw.mix, mixCounts(sw.mix, sw.members.map((m) => sw.rows[m])));
  await learnStarted(await Daemon.swarmAdd(sw.name, row), 1);
}
// Stopped, the swarm refuses its agents' posts, so nothing wakes them; your next post resumes it.
async function stopSwarm(sw) {
  const r = await Daemon.swarmStop(sw.name); learnSwarm(r.swarm);
  const failed = r.failed?.[0];
  toast(failed ? `stop: ${failed.agent}: ${failed.error}` : 'stopped every agent; your next post resumes the swarm', failed ? 6000 : 4000);
}
// You decide an open proposal: approving opens its stream with its proposer as lead.
async function decide(sw, id, approve) {
  const r = await Daemon.swarmDecide(sw.name, id, approve, '');
  await readBoard(sw);
  toast(`${id} ${r.decided ?? (approve ? 'approved' : 'denied')}`, 2200);
}
async function postToSwarm(sw, text) {
  const r = await Daemon.swarmPost(sw.name, text);
  sw.stopped = false; readBoard(sw);
  const reached = (r.steered?.length ?? 0) + (r.woke?.length ?? 0);
  toast(r.missed?.length ? `missed ${r.missed.map((x) => `${x.agent}: ${x.error}`).join('; ')}` : `posted · reached ${reached} agent${reached === 1 ? '' : 's'}`, r.missed?.length ? 6000 : 2200);
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
      if (S.autoSelect) { const first = tree().find((n) => n.b); if (first) S.selected = first.b.name; }
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
      if (data.from?.bot) S.turnFrom.set(`${name}\u0000${turn}`, data.from.bot);
      else if (typeof data.origin === 'string') S.turnOrigin.set(`${name}\u0000${turn}`, data.origin);
      const b = bot(name); if (b) { b.status = 'running'; b.runningTurn = turn; b.waitingOn = []; b.turnStarted = S.live ? Date.now() : 0; b.elapsed = 0; }
      // The event that puts a prompt on the lineage names who sent it, here and on `steered`.
      const t = transcript(name), by = senderOf(data);
      if (typeof data.node === 'number') pushNode(t, { kind: 'node', node: data.node, turn, ...(by ? { by } : {}) });
      break;
    }
    case 'queued': {
      if (data.from?.bot) S.turnFrom.set(`${name}\u0000${turn}`, data.from.bot);
      else if (typeof data.origin === 'string') S.turnOrigin.set(`${name}\u0000${turn}`, data.origin);
      // `ready` waits for a daemon-wide slot with nothing else running on the bot; `queued` sits behind its own turn.
      const b = bot(name); const behindOwn = !!b && (b.runningTurn !== null || isActive(b.status));
      if (b && !behindOwn) { b.status = data.status ?? 'queued'; b.runningTurn = turn; }
      const t = transcript(name);
      addItem(t, { kind: 'note', text: data.delivery === 'steer' ? 'steers in at the running turn\'s next step' : behindOwn ? 'queued behind the running turn' : 'queued for a slot', turn });
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
      const row = { kind: 'tool', from: t.callNode, callId: data.call_id, name: tname, summary: callSummary(tname, args), path: toolPath(tname, parsed), background: tname === 'shell' && parsed.background === true, done: false, started: S.live ? Date.now() : 0, took: 0, turn };
      let existing = null;
      for (let i = t.items.length - 1; i >= 0; i--) {
        const it = t.items[i]; if (it.turn !== turn) break;
        if (it.kind === 'tool' && it.callId === data.call_id) { existing = it; break; }
      }
      if (existing) {
        row.from = existing.from ?? row.from;
        if (data.arguments_truncated) { row.summary = existing.summary; row.path = existing.path; row.background = existing.background; }
        Object.assign(existing, row); t.gen += 1; } else addItem(t, row);
      break;
    }
    case 'tool_completed': {
      const t = transcript(name);
      let call = null;
      for (let i = t.items.length - 1; i >= 0; i--) { const it = t.items[i]; if ((it.kind === 'tool' || it.kind === 'tool_stub') && it.turn === turn && it.callId === data.call_id) { call = it; break; } }
      if (call) { call.done = true; if (call.started) call.took = Date.now() - call.started; call.started = 0; patchRun(name, call); }
      const shown = S.ui.file;
      if (shown && call?.path && (call.name === 'write' || call.name === 'edit') && joinPath(bot(name)?.workspace ?? S.config?.workspace ?? '', call.path) === shown.full) openFile(shown.bot, shown.full);
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
    case 'turn_waiting': {
      const b = bot(name); if (b) { b.status = 'waiting'; b.waitingOn = data.handles ?? []; }
      // An approval only the person can give is news for the task's coordinator.
      if (data.approval && S.live) { if (S.snapshot) S.heldNews.push([name, turn, 'waiting for approval', null, data.call_id]); else tellLead(name, turn, 'waiting for approval', null, data.call_id); }
      break;
    }
    case 'turn_paced': { const b = bot(name); if (b) b.status = 'paced'; break; }
    case 'turn_resumed': { const b = bot(name); if (b) { b.status = 'running'; b.waitingOn = []; } break; }
    case 'steered': {
      // The steer's message joins this turn; `steer` is the steer's own turn.
      const t = transcript(name), by = senderOf(data);
      if (typeof data.node === 'number') pushNode(t, { kind: 'node', node: data.node, turn, ...(by ? { by } : {}) });
      else addItem(t, { kind: 'note', text: 'steered into the running turn', turn });
      if (!data.from?.bot) S.wanted.add(`${name}\u0000${turn}`);
      break;
    }
    case 'turn_finished': {
      const status = data.status ?? '?';
      const b = bot(name);
      const key = `${name}\u0000${turn}`, from = S.turnFrom.get(key), origin = S.turnOrigin.get(key); S.turnFrom.delete(key); S.turnOrigin.delete(key);
      // A steer absorbed into a running turn finishes as its own turn while that turn goes on.
      if (b && (b.runningTurn === null || b.runningTurn === turn)) { b.runningTurn = null; b.waitingOn = []; if (b.turnStarted) b.elapsed = Date.now() - b.turnStarted; b.turnStarted = 0; b.status = status === 'completed' || status === 'steered' ? 'idle' : status; }
      const t = transcript(name);
      if (t.streamingTurn === turn) { t.text = ''; t.thinking = ''; t.thinkingSince = 0; t.thinkingMs = 0; t.streamingTurn = null; t.streamGen += 1; }
      if (status !== 'completed' && status !== 'steered') addItem(t, { kind: 'note', text: data.error ? `${status}: ${data.error}${data.detail ? ': ' + data.detail : ''}` : status, turn });
      // A turn another bot asked for is that bot's news, not the person's, unless the person steered into it.
      const wanted = S.wanted.delete(key);
      if (b && S.live && status === 'completed' && (!from || wanted) && !onScreen(name) && !S.unseen.has(name)) { S.unseen.add(name); patchUnseen([name]); }
      // A steer's turn is part of the turn it joined, whose end is the news.
      // A task's creator may still be on a snapshot page to come; its news waits for the whole snapshot.
      if (S.live && status !== 'steered') { if (S.snapshot) S.heldNews.push([name, turn, status, from, undefined, origin]); else tellLead(name, turn, status, from, undefined, origin); }
      // A coordinator coming to rest hears what waited for it.
      if (S.wakes.has(name)) wakeSoon(name);
      // A background command may outlive its turn; only a wait result says how it ended.
      break;
    }
    case 'deleted': {
      if (S.snapshot) S.deleted.add(name);
      // A later bot may take the name as a swarm's agent, so it is looked for again.
      looked.delete(name);
      const p = bot(name)?.project;
      forgetBot(name);
      if (S.selected === name) S.selected = p && S.bots.has(p + LEAD) ? p + LEAD : S.bots.keys().next().value ?? '';
      // A deleted agent leaves its swarm, which stops counting it and posting to it.
      if (S.memberOf.has(name)) {
        const sw = S.memberOf.get(name); patchRailRow(swarmKey(sw));
        leave(sw, name);
      }
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
  return { kind: 'tool', name, callId, summary: callSummary(name, args), path: toolPath(name, parsed), background: name === 'shell' && parsed.background === true, done: true, started: 0, took: 0 };
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
    // The daemon names who sent a prompt with its item.
    if (row && !it.by) { const by = senderOf(row); if (by) it.by = by; }
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
  // A swarm's agents show their last lines on its Agents tab, so the first dozen load.
  const sw = swarmOf(S.selected);
  if (sw) { if (sw.tab === 'agents') for (const m of sw.members.slice(0, 12)) if (memberBot(sw, m) && m !== S.ui.side) await load(m); }
  else await load(S.selected);
  if (S.ui.side && S.ui.side !== S.selected) await load(S.ui.side);
  // Cards on screen show their peer's last line, so those peers load too.
  for (const who of peers().slice(-12)) if (who !== S.selected && who !== S.ui.side) await load(who);
}

// All transcript mutations, including snapshot pages and creation replies,
// share the event/load queue. A snapshot must not replace ranges underneath
// a pending history read, and an old reply must not mutate a new session.
let chain = Promise.resolve();
function enqueue(job) { chain = chain.then(job, job); return chain; }

// ---------- coordinator wake ----------
// Work goes on in tasks a project's coordinator started, by its own asks and by you working in them
// directly. The coordinator hears of it: once it rests, and at most every WAKE_MS, one message lists the
// tasks that ended turns or wait for an approval since it last heard, by the handles its wait tool reads
// them with, and its role says what to do with that. A turn it is waiting on is not news, since its wait
// reads it; nor is its own fork or side chat. Only live turns count, while this window is attached.
// Turns you asked for in a task yourself are its `theirs` news, listed apart: the coordinator hears of
// them but is not asked to act on them. Turns another agent or the app (a schedule, by its `origin`)
// asked for, and approvals, are its `act` news; a turn's end goes where its pending approval is.
// Each kind keeps its own first and latest turn and a count, whatever the backlog, so a task can be in
// both lists with the handles each one needs; one message names at most WAKE_TASKS tasks, those with
// news to act on first, and the rest wait for the next.
const WAKE_MS = 10 * 60 * 1000, WAKE_TASKS = 32, KINDS = ['act', 'theirs'];
function tellLead(name, turn, status, from, approval, origin) {
  const b = bot(name), lead = b && creatorOf(b);
  if (!lead || !leadProject(lead.name) || name.startsWith(`${lead.name}-`)) return;
  if (lead.waitingOn?.includes(`turn:${name}/${turn}`)) return;
  let w = S.wakes.get(lead.name);
  if (!w) { w = { tasks: new Map(), last: 0, timer: null }; S.wakes.set(lead.name, w); }
  const by = status === 'waiting for approval' ? null : from === lead.name ? 'you' : from ?? origin ?? 'the person';
  const asked = w.tasks.get(name)?.act, answered = asked?.turn === turn && asked.status === 'waiting for approval';
  merge(w.tasks, name, { [by === 'the person' && !answered ? 'theirs' : 'act']: { first: turn, turn, status, by, count: 1, ...(approval ? { approval } : {}) } });
  wakeSoon(lead.name);
}
function merge(tasks, name, news) {
  const had = tasks.get(name) ?? {}, out = {};
  for (const k of KINDS) {
    const [a, t] = [had[k], news[k]];
    const v = a && t ? { ...(t.turn >= a.turn ? t : a), first: Math.min(a.first, t.first), count: a.count + t.count } : a ?? t;
    if (v) out[k] = v;
  }
  tasks.set(name, out);
}
// A working coordinator waits for its turn to end, which calls this again.
function wakeSoon(lead) {
  const w = S.wakes.get(lead), l = bot(lead);
  if (!w || w.timer || !w.tasks.size || !l || isActive(l.status)) return;
  w.timer = setTimeout(() => { w.timer = null; wake(lead); }, Math.max(0, w.last + WAKE_MS - Date.now()));
}
async function wake(lead) {
  const w = S.wakes.get(lead), l = bot(lead);
  if (!w || !w.tasks.size || !S.attached || !l || l.id == null || isActive(l.status)) return;
  const sent = [...w.tasks].sort(([, a], [, b]) => (b.act ? 1 : 0) - (a.act ? 1 : 0)).slice(0, WAKE_TASKS);
  const items = sent.flatMap(([name, n]) => KINDS.filter((k) => n[k]).map((k) => [name, k, n[k]]));
  for (const [name] of sent) w.tasks.delete(name);
  w.last = Date.now();
  // Key the latest notification per task and kind, including its phase and
  // approval call: one turn can need several approvals before its completion.
  // Windows with the same news still deduplicate, regardless of earlier turn
  // counts; a window that saw news of another kind sends its own message.
  const prompt = wakeText(items, w.tasks.size);
  const key = ([name, k]) => `${name}\u0000${k}`;
  const id = `app-wake-${l.id}-${digest(JSON.stringify(items.map(([name, k, t]) => [name, k, t.turn, t.status, t.approval ?? null]).sort((a, b) => key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0)))}`;
  try { await Daemon.request('submit', { bot: lead, bot_id: l.id, request_id: id, prompt, delivery: 'queue', origin: 'tasks' }); }
  catch (e) {
    if (/^bot_not_found/.test(e?.message ?? '')) { S.wakes.delete(lead); return; }
    // Another window told it first.
    if (/^idempotency_conflict/.test(e?.message ?? '')) return;
    const later = w.tasks;
    w.tasks = new Map(sent);
    for (const [name, t] of later) merge(w.tasks, name, t);
    Daemon.log?.(`wake ${lead}: ${e?.message ?? e}`);
    wakeSoon(lead);
  }
}
// FNV-1a over the text's UTF-16 units, 64 bits as hex.
function digest(text) {
  let h = 0xcbf29ce484222325n;
  for (let i = 0; i < text.length; i++) h = BigInt.asUintN(64, (h ^ BigInt(text.charCodeAt(i))) * 0x100000001b3n);
  return h.toString(16).padStart(16, '0');
}
function wakeText(items, more) {
  const line = ([name, , t]) => `- ${name}: turn:${name}/${t.turn} ${t.status}${t.by ? `, asked by ${t.by}` : ''}${t.count > 1 ? `, and ${t.count - 1} earlier since turn:${name}/${t.first}` : ''}`;
  const lines = items.filter(([, k]) => k === 'act').map(line), theirs = items.filter(([, k]) => k === 'theirs').map(line);
  if (theirs.length) lines.push('The person asked for these turns in the task themselves, so they are theirs:', ...theirs);
  if (more) lines.push(`- ${more} more tasks in the next update`);
  return `Task updates: since you last heard, tasks you started ended turns or wait for an approval. The wait tool reads each one's final reply by its handle.\n${lines.join('\n')}`;
}

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
  const sw = ev.bot && (swarmOfBot(ev.bot) ?? (['usage', 'turn_finished'].includes(ev.event) ? swarmOfHelper(ev.bot) : null));
  if (sw) { if (FLEET_EVENTS.has(ev.event)) patchRailRow(swarmKey(sw.name)); if (ev.durable !== false) boardSoon(sw, ev.event === 'turn_finished'); if (ev.event === 'turn_finished' || (ev.event === 'usage' && usageDue(sw, ev.data))) checkSoon(sw); }
  else if (S.live && BRIEFED.has(ev.event) && SWARM_AGENT.test(ev.bot ?? '')) swarmsSoon(ev.bot);
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
  learnEffort(b, record);
  learnFamily(b, record);
  learnWorkspace(b, record);
  if (record.created_by) { b.parent = record.created_by; b.parentId = record.created_by_id ?? null; }
  seedHistory(record);
}
let attaching = null, retryTimer = null;
function retryAttach(delay = 2000) {
  clearTimeout(retryTimer);
  retryTimer = setTimeout(() => { retryTimer = null; if (!S.attached && !S.replacing) attach(); }, delay);
}
function attach() {
  if (!attaching) {
    clearTimeout(retryTimer); retryTimer = null;
    attaching = attachOnce().finally(() => {
      attaching = null;
      if (!S.attached && !idle()) retryAttach();
    });
  }
  return attaching;
}
async function attachOnce() {
  try {
    if (!S.config) S.config = await Daemon.setup();
    let { session, store, workspace } = await Daemon.attach(S.cursor);
    // Another store answers where the last one did: its cursor, bot ids and names mean other things,
    // so nothing learned from the last one is kept, and its log is followed from the start.
    if (S.store && store && store !== S.store) { forgetStore(); ({ session, store, workspace } = await Daemon.attach(0)); }
    S.session = session; S.store = store ?? null;
    // A window on a host starts in the home the host named, unless it was given a folder there.
    if (!S.config.workspace && workspace) { S.config.workspace = workspace; S.homeWorkspace = true; }
    S.deleted = new Set(); S.snapshot = true; S.heldNews = [];
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
      for (const news of S.heldNews.splice(0)) tellLead(...news);
      S.attached = true;
      // What waited while detached goes out now, each window permitting.
      for (const lead of S.wakes.keys()) wakeSoon(lead);
      try { await loadSwarms(); } catch (e) { toast(`swarms: ${e?.message ?? e}`, 5000); }
      restore();
      // A selection deleted while detached had no `deleted` event to replay; show a surviving bot.
      if (!isOpen(S.selected)) { const first = tree().find((n) => n.b); S.selected = first ? first.b.name : ''; }
      const shown = swarmOf(S.selected); if (shown) { readBoard(shown); readUsage(shown); }
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
// Everything the window learned from one store, dropped before it shows another.
function forgetStore() {
  S.cursor = 0; S.bots.clear(); S.transcripts.clear(); S.drafts.clear(); S.override.clear(); S.effort.clear(); S.families.clear();
  S.swarms.clear(); S.memberOf.clear(); S.deleted.clear(); looked.clear();
  for (const w of S.wakes.values()) clearTimeout(w.timer);
  S.wakes.clear(); S.turnFrom.clear(); S.turnOrigin.clear(); S.heldNews = []; S.unseen.clear(); S.wanted.clear();
  S.selected = ''; S.autoSelect = true; S.ui.side = null; S.ui.folded = new Set(); dropFile();
  S.botsGen += 1; S.shapeGen += 1;
  // A home the last host named is not this one's.
  if (S.homeWorkspace) { S.config.workspace = null; S.homeWorkspace = false; }
}
// No provider to run: starting again cannot help until Settings changes, which attaches itself.
// Attaching again cannot help until something changes: a provider in Settings, or a newer app.
function idle() { return /^no_provider/.test(S.lastReason ?? '') || olderDaemon(S.lastReason) === 'newer'; }
// A daemon from before an upgrade still owns the socket: it speaks an older protocol, and the app can
// stop it and start its own. One newer than the app is left alone.
function olderDaemon(reason) {
  // The app names the age by code, from what the daemon announced, not from the error's wording.
  return /^daemon_older\b/.test(reason ?? '') ? 'older' : /^daemon_newer\b/.test(reason ?? '') ? 'newer' : null;
}
function showDetached(reason) {
  S.attached = false;
  // Only a daemon this window starts for its store can be replaced; one on a socket it was given is
  // stopped with its own agent, and the window attaches once it is gone.
  const age = olderDaemon(reason), ours = !!Daemon.replaceDaemon && S.config?.managed !== false;
  const settings = '<button type="button" class="sbtn" data-act="settings">Open Settings</button>';
  const what = age === 'older' && ours ? `<div class="why">A daemon from before this update is still running.</div><div style="margin-top:12px"><button type="button" class="sbtn primary" data-act="replace-daemon">Restart the daemon</button> ${settings}</div><div class="hint">Turns it is running end as interrupted. Every chat is kept.</div>`
    : age === 'older' ? `<div class="why">A daemon from before this update holds this socket, and this window did not start it: stop it with its own agent (agent shutdown), then start one from this update's agent. The window attaches when it answers.</div><div style="margin-top:12px">${settings}</div>`
    : `${age === 'newer' ? '<div class="why">The daemon is newer than this app: update the app.</div>' : ''}<div style="margin-top:12px">${settings}</div>`;
  const state = age === 'newer' ? 'stopped' : idle() ? 'waiting for a provider' : 'retrying';
  const where = S.config?.host ? `daemon on <span class="k">${esc(S.config.host)}</span>` : `daemon at <span class="k">${esc(S.config?.socket ?? '?')}</span>`;
  $('detached').innerHTML = `<div><b>not attached</b></div><div>${esc(reason)}</div><div style="margin-top:8px">${where} · ${state}</div>${what}`;
  $('detached').classList.add('on');
  if (!idle()) retryAttach();
  else { clearTimeout(retryTimer); retryTimer = null; }
}
function restore() {
  const key = sessionKey(); if (!key) return;
  let saved = null; try { saved = JSON.parse(localStorage.getItem(key) || 'null'); } catch (_) {}
  if (!saved) return;
  if (saved.selected && isOpen(saved.selected)) { S.selected = saved.selected; S.autoSelect = false; }
  if (saved.side && S.bots.has(saved.side) && saved.side !== S.selected) S.ui.side = saved.side;
  S.ui.rail = saved.rail !== false; S.ui.steps = !!saved.steps;
  if (Array.isArray(saved.folded)) { S.ui.folded = new Set(saved.folded.filter((p) => typeof p === 'string')); S.shapeGen += 1; }
  // A model pick belongs to the identity it was made for, not to whichever bot holds the name now.
  if (Array.isArray(saved.override)) for (const entry of saved.override) {
    const [name, id, model] = Array.isArray(entry) ? entry : [];
    const b = bot(name);
    if (b && b.id != null && b.id === id && typeof model === 'string' && runsOn(b, model)) S.override.set(name, model);
  }
  if (Array.isArray(saved.effort)) for (const entry of saved.effort) {
    const [name, id, level] = Array.isArray(entry) ? entry : [];
    const b = bot(name);
    if (b && b.id != null && b.id === id && effortsFor(b.model).includes(level) && level !== b.reasoning) S.effort.set(name, level);
  }
}
function save() { const key = sessionKey(); if (!key) return; try { localStorage.setItem(key, JSON.stringify({ selected: S.selected, side: S.ui.side, rail: S.ui.rail, steps: S.ui.steps, folded: [...S.ui.folded], override: [...S.override].map(([name, model]) => [name, bot(name)?.id ?? null, model]), effort: [...S.effort].map(([name, level]) => [name, bot(name)?.id ?? null, level]) })); } catch (_) {} }
window.addEventListener('beforeunload', save);
window.addEventListener('focus', markSeen);

// ---------- render ----------
function inline(text) {
  return esc(text).replace(/\*\*(.+?)\*\*/g, '<h>$1</h>').replace(/`([^`]+)`/g, '<code>$1</code>');
}
// A message's Markdown, drawn once and kept with the item: a pane drawn again reuses it, and it is
// drawn anew only when its text changes or, for one whose code waited, highlighting arrives (see
// `Rich.onReady`). What it keeps counts toward the transcript's decoded bytes, so the window's bound
// holds.
function textHTML(it, t) {
  if (it.htmlOf !== it.text || (it.htmlWaited && it.htmlAt !== Rich.version)) {
    const html = `<div class="md">${Rich.html(it.text)}</div>`;
    const d = 2 * (html.length - (it.html?.length ?? 0)); it.bytes = (it.bytes || 0) + d; if (t) t.bytes = Math.max(0, (t.bytes || 0) + d);
    it.html = html; it.htmlOf = it.text; it.htmlAt = Rich.version; it.htmlWaited = Rich.waited;
  }
  return it.html;
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
  return { attr: `data-task="${esc(p.name)}"`, task: p.name, status: shownStatus(p), name: shortName(p), last: lastLine(transcript(who)), elapsed: el, sel: S.ui.side === who || S.selected === who };
}
// The two panes that show a transcript: the main thread and the one beside it.
const PANES = [['log', () => S.selected], ['side', () => S.ui.file ? null : S.ui.side]];
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
    const now = n === 1 || last.started ? `<b>${esc(last.name)}</b> ${summaryHTML(last)}${el}` : esc([...new Set(tools.map((it) => it.name))].join(' · '));
    head = n === 1 ? `<span class="now">${now}</span>` : `${n} steps <span class="now">${now}</span>`;
  }
  let err = null; for (const it of items) if (it.kind === 'out' && it.err) err = it.err;
  if (err) head += ` <span class="err">✘ ${esc(err)}</span>`;
  const body = open ? `<div class="body">${items.map((it, k) => stepHTML(it, s + k)).join('')}</div>` : '';
  return { html: `<div class="steps" data-i="${s}"><div class="sum" data-run="${s}" role="button" tabindex="0">${open ? '▾' : '▸'} ${head}</div>${body}</div>`, end };
}
// A read, write or edit names its path; it opens that file beside.
const summaryHTML = (it) => it.path ? `<a class="fpath" href="#" data-file="${esc(it.path)}">${esc(it.summary)}</a>` : esc(it.summary);
function stepHTML(it, i) {
  switch (it.kind) {
    case 'thought': return `<div class="line think">${esc(it.text)}</div>`;
    case 'tool': { const el = it.started ? `<span class="el" data-started="${it.started}">${fmt(Date.now() - it.started)}</span>` : it.took >= 1500 ? `<span class="el">${fmt(it.took)}</span>` : ''; return `<div class="line tool" data-call="${esc(it.callId)}">▸ <b>${esc(it.name)}</b> ${summaryHTML(it)}${el}</div>`; }
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

function itemHTML(it, t = null) {
  switch (it.kind) {
    case 'user': {
      if (!it.by) return `<div class="line user">› ${esc(it.text)}</div>`;
      const sender = it.by.bot ? senderBot(it.by) : null;
      const tag = sender ? `<button type="button" class="by" data-task="${esc(it.by.bot)}" title="Sent by ${esc(it.by.bot)}, turn ${esc(it.by.turn)}">${esc(agentName(it.by.bot, sender))}</button>`
        : `<span class="by"${it.by.bot ? ` title="Sent by ${esc(it.by.bot)}, turn ${esc(it.by.turn)}, since deleted"` : ''}>${esc(it.by.bot ? agentName(it.by.bot, null) : it.by.app)}</span>`;
      return `<div class="line user agent">${tag} ${esc(it.text)}</div>`;
    }
    case 'text': return textHTML(it, t);
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
    h += itemHTML(it, t); i++;
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
    const done = kind === 'text' ? document.createElement('div') : null; if (done) done.className = 'md';
    el.replaceChildren(...(kind || running ? [done, line].filter(Boolean) : []));
    state = { transcript: t, kind, turn: t.streamingTurn, gen: t.streamGen, offset: 0, text, running, done, cut: {}, drawn: 0, used: { lines: 0, tags: 0, code: 0 } };
    tails.set(el, state);
  }
  if (value.length <= state.offset) return;
  // Streamed text is drawn block by block: what has ended (a paragraph, a closed fence) is drawn
  // once and appended, and only the block still being written is plain text. Each delta reads its
  // own characters, never the whole reply, and provider text never becomes markup unparsed. The
  // blocks share one message's bounds (`used`); past them the rest streams as plain text.
  const at = state.done && !state.used.over ? Rich.cut(state.cut, value) : 0;
  if (at > state.drawn) {
    const box = document.createElement('div'); box.innerHTML = Rich.html(value.slice(state.drawn, at), state.used);
    const added = [...box.childNodes]; state.done.append(...added);
    for (const n of added) if (n.nodeType === 1) Rich.hydrate(n);
    state.text.data = value.slice(at); state.drawn = at;
  } else state.text.appendData(value.slice(state.offset));
  state.offset = value.length;
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
  const rebuild = rendered < 0 || rendered > t.items.length || !tail || !tail.classList.contains('tail');
  let added = '', from = rendered, old = null; const was = t.bytes || 0;
  if (!rebuild && rendered < t.items.length) {
    const next = t.items[rendered], prev = t.items[rendered - 1];
    if (prev && inRun(prev, next.turn) && inRun(next, next.turn)) {
      const s = runStart(t, rendered - 1);
      old = STEP.has(t.items[s].kind) ? el.querySelector(`.steps[data-i="${s}"]`) : null;
      if (old) from = s;
    }
    added = itemsHTML(t, from);
  }
  // Drawn messages count toward the transcript's bytes, so drawing can pass the bound: the window then
  // folds what it lets go, and the pane is drawn from what is left (kept messages reuse their HTML).
  const over = () => t.bytes > DECODE_BYTES && t.bytes > was;
  if (rebuild || over()) {
    let html = itemsHTML(t);
    if (over()) { evict(t); html = itemsHTML(t); }
    el.innerHTML = html + '<div class="tail"></div>';
    el.dataset.key = paneKey(name, t);
    Rich.hydrate(el);
    tail = el.lastElementChild;
    // History loaded above the reader keeps their place instead of shoving it down.
    if (!atBottom) el.scrollTop += el.scrollHeight - before;
  } else if (added) {
    if (old) { const sep = old.previousElementSibling; if (sep?.dataset?.sep === String(from)) sep.remove(); old.remove(); }
    // Only what was just added is looked through for blocks to draw.
    const prev = tail.previousElementSibling; tail.insertAdjacentHTML('beforebegin', added);
    for (let n = prev ? prev.nextElementSibling : el.firstElementChild; n && n !== tail; n = n.nextElementSibling) Rich.hydrate(n);
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

// ---------- files ----------
// A file a message links or a step read, wrote or edited, opened beside: read from the agent's
// folder by the core, drawn by its kind (see `Rich.file`). It takes the place of the pane beside
// until closed, and is read again when a step of the agent it came from writes or edits it.
const FILE_CAP = 4 * 1024 * 1024;
// `~/` is the home folder, as the core reads it; any other name, `~notes.md` too, is the folder's.
function joinPath(dir, path) {
  const parts = [];
  for (const seg of (path.startsWith('/') || path.startsWith('~/') || !dir ? path : `${dir.replace(/\/+$/, '')}/${path}`).split('/')) {
    if (seg === '..' && parts.length && parts.at(-1) !== '..' && parts.at(-1) !== '') parts.pop(); else if (seg !== '.' && (seg || !parts.length)) parts.push(seg);
  }
  return parts.join('/') || '/';
}
const dirOf = (path) => path.replace(/\/[^/]*$/, '') || '/';
// Which agent's folder a click names a path in: the file beside's own folder, or the agent in that pane.
function openFileFrom(path, el) {
  const beside = el?.closest?.('.pane.side');
  if (beside && S.ui.file) return openFile(S.ui.file.bot, joinPath(dirOf(S.ui.file.full), path));
  const who = beside ? S.ui.side : S.selected, b = bot(who);
  return openFile(who, joinPath(b?.workspace ?? S.config?.workspace ?? '', path));
}
async function openFile(who, full) {
  const old = S.ui.file;
  if (old?.url) URL.revokeObjectURL(old.url);
  const f = S.ui.file = { bot: who, full, gen: (old?.gen ?? 0) + 1, state: 'loading', view: null, url: null };
  render();
  try {
    const bytes = new Uint8Array(await Daemon.readFile(full));
    if (S.ui.file !== f) return;
    Object.assign(f, { state: 'ok', bytes: bytes.subarray(0, FILE_CAP), more: bytes.length > FILE_CAP, gen: f.gen + 1 });
  } catch (e) { if (S.ui.file !== f) return; Object.assign(f, { state: 'error', error: String(e?.message ?? e), gen: f.gen + 1 }); }
  render();
}
// A file opened from an agent goes with that agent, and with the store it came from.
function dropFile() {
  const f = S.ui.file; if (!f) return false;
  if (f.url) URL.revokeObjectURL(f.url);
  S.ui.file = null; $('side').dataset.key = ''; $('side').dataset.who = ''; $('sidetitle').dataset.k = '';
  return true;
}
// The chat it covered is on screen again, and what it finished meanwhile is seen.
function closeFile() { if (dropFile()) { render(); markSeen(); focusInput(S.ui.side ? 'side' : 'main'); } }
function renderFile() {
  const f = S.ui.file, el = $('side');
  // Drawn when read, and again when highlighting arrives for code that waited for it; a page,
  // diagram or chart beside keeps running as it is.
  if (f.state === 'ok' && (f.at !== f.gen || (f.waited && f.ver !== Rich.version))) {
    if (f.url) URL.revokeObjectURL(f.url);
    const shown = Rich.file(f.full, f.bytes, f.more);
    Object.assign(f, { view: shown.html, url: shown.url ?? null, waited: shown.waited, at: f.gen, ver: Rich.version });
  }
  const key = `file|${f.full}|${f.gen}|${f.ver}`;
  if (el.dataset.key === key) return;
  const name = f.full.split('/').pop(), where = dirOf(f.full).replace(/^\/(Users|home)\/[^/]+/, '~');
  $('sidetitle').dataset.k = key;
  $('sidetitle').innerHTML = `<div class="crumbs"><b>${esc(name)}</b><span class="branch" title="${esc(f.full)}">${esc(where)}</span></div><div class="tools"><button type="button" class="ibtn" data-act="close-file" title="Close (Esc)" aria-label="Close">✕</button></div>`;
  el.innerHTML = `<div class="fview">${f.state === 'loading' ? '<div class="line pending">reading…</div>' : f.state === 'error' ? `<div class="line out bad">${esc(f.error)}</div>` : f.view}</div>`;
  el.dataset.key = key; el.dataset.who = ''; el.scrollTop = 0;
  // A chart fills the pane's width, so it is measured once the pane has finished opening.
  const opening = $('app')?.getAnimations?.() ?? [];
  if (!opening.length) Rich.hydrate(el);
  else Promise.all(opening.map((a) => a.finished.catch(() => {}))).then(() => { if (el.dataset.key === key) Rich.hydrate(el); });
}

// ---------- heads and composers ----------
function headHTML(b, pane) {
  const waiting = b.waitingOn.length ? ` on ${esc(waitSummary(b))}` : '';
  const state = `<span class="glyph ${b.status}">${glyphOf(b.status)}</span><span class="state">${labelOf(b.status)}${waiting}</span>`;
  if (pane === 'side') return `<div class="crumbs"><b>${esc(shortName(b))}</b>${branchHTML(b)}${state}</div><div class="tools">${moreButton(b.name)}<button type="button" class="ibtn" data-act="swap" title="Full view" aria-label="Full view">⤢</button><button type="button" class="ibtn" data-act="close-side" title="Close (Esc)" aria-label="Close">✕</button></div>`;
  const lead = b.project ? bot(b.project + LEAD) : null, sw = swarmOfBot(b.name);
  // A swarm's agent goes back to its swarm.
  const crumbs = sw ? `<button type="button" class="back" data-act="open" data-who="${esc(swarmKey(sw.name))}" title="Back to the swarm">← ⁂ ${esc(memberShort(sw, sw.name))}</button><span class="sep">/</span><b>${esc(shortName(b))}</b>`
    : !lead ? `<b>${esc(b.name)}</b>` : lead === b ? `<b>${esc(b.project)}</b>`
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
  el.innerHTML = b ? headHTML(b, pane) : pane === 'main' ? '<div class="crumbs"><span class="state">no bots · /new NAME PROVIDER/MODEL creates one</span></div>' : '';
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
// The effort an agent's next turn runs at: one picked for its turns, else its own.
const effortOf = (b) => S.effort.get(b.name) ?? b.reasoning ?? null;
function renderComposer(pane, b, sw = null) {
  const ids = PANE[pane], mode = sendMode(b), model = b ? modelOf(b) : '', effort = b ? effortOf(b) : null;
  const key = sw ? `${SWARM}${sw.name}` : b ? `${b.name}|${mode}|${model}|${effort}|${b.runningTurn !== null}` : '-';
  const send = $(ids.send); if (send.dataset.k === key) return; send.dataset.k = key;
  const caret = $(ids.form).querySelector?.('.caret'); if (caret) caret.hidden = !!sw;
  // On a swarm the composer posts to its board: no model, no stop, one way to send.
  if (sw) { send.textContent = 'Post'; $(ids.model).hidden = true; $(ids.stop).hidden = true; $(ids.input).placeholder = 'Post to the board · @name wakes that agent'; return; }
  send.textContent = ACTION[mode];
  $(ids.model).textContent = b ? `${model.split('/').slice(1).join('/') || model}${effort ? ` · ${effort}` : ''} ▾` : '';
  $(ids.model).hidden = !b; $(ids.stop).hidden = !b || b.runningTurn === null;
  $(ids.input).placeholder = !b ? (pane === 'main' ? '/new NAME PROVIDER/MODEL [EFFORT]' : '') : mode === 'queue' ? 'queues after this turn' : mode === 'steer' ? 'steers into this turn' : mode === 'side' ? 'asks a side chat' : '';
}

// ---------- swarm view ----------
// A swarm's head: where it sits, how many of its agents work, its tokens against its budget, and its
// tabs. The Board is its posts; Agents are its agents as cards, which open beside. A swarm with a
// council also has the Council, its proposals and their votes, and Streams, the approved ones.
const tokens = (n) => (n >= 1e6 ? `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1).replace(/\.0$/, '')}M` : n >= 1e3 ? `${Math.round(n / 1e3)}k` : String(n));
function renderSwarmHead(el, sw) {
  const working = sw.members.filter((m) => RUNNING.has(memberBot(sw, m)?.status)).length;
  const open = sw.state.proposals.filter((p) => p.status === 'open').length, streams = sw.state.proposals.filter((p) => p.status === 'approved').length;
  const key = `${SWARM}${sw.name}|${working}|${sw.members.length}|${sw.stopped}|${sw.used}|${sw.tab}|${open}|${streams}|${sw.filter}`;
  if (el.dataset.k === key) return; el.dataset.k = key;
  const lead = bot(sw.project + LEAD), st = swarmStatus(sw);
  const back = lead ? `<button type="button" class="back" data-act="open" data-who="${esc(lead.name)}" title="Back to the coordinator">← ${esc(sw.project)}</button><span class="sep">/</span>` : '';
  const state = sw.stopped ? 'stopped' : working ? `${working} of ${sw.members.length} working` : `${sw.members.length} agents`;
  const budget = `${sw.used == null ? '' : tokens(sw.used) + ' of '}${tokens(sw.budget)} tokens`;
  const tab = (v, label) => `<button type="button" class="tab${sw.tab === v ? ' on' : ''}" data-act="swarm-tab" data-v="${v}">${label}</button>`;
  const filter = sw.filter && sw.tab === 'board' ? `<button type="button" class="tag on" data-act="swarm-filter" data-v="" title="Show every post">#${esc(sw.filter)} ×</button>` : '';
  const council = sw.council ? `${tab('council', `Council${open ? ` <span class="count">${open}</span>` : ''}`)}${tab('streams', `Streams ${streams}`)}` : '';
  el.innerHTML = `<div class="crumbs">${back}<b>⁂ ${esc(memberShort(sw, sw.name))}</b><span class="glyph ${st}">${glyphOf(st)}</span><span class="state">${esc(state)}</span><span class="branch">${esc(budget)}</span></div><div class="tools">${filter}${tab('board', 'Board')}${council}${!sw.council ? tab('streams', 'Work') : ''}${tab('agents', `Agents ${sw.members.length}`)}${moreButton(swarmKey(sw.name))}</div>`;
}
const streamTag = (stream) => `<button type="button" class="tag" data-act="swarm-filter" data-v="${esc(stream)}">#${esc(stream)}</button>`;
// A proposal's votes so far against the seats' majority.
// Only the seats as they are now count: a seat that left gives its place, and its vote, to the next agent.
function tally(sw, p) {
  const seats = new Set(sw.seats.map((m) => memberShort(sw, m)));
  const votes = Object.entries(p.votes ?? {}).filter(([seat]) => seats.has(seat)).map(([, v]) => v);
  const yes = votes.filter((v) => v.yes).length, no = votes.length - yes;
  return `${yes} yes${no ? ` · ${no} no` : ''} of ${sw.council}`;
}
// A board line: who, then the post, with the agents it names marked and its stream as a tag that
// filters the board; an agent's name opens it beside. Roles, votes, decisions and joins are quieter.
function postHTML(sw, line) {
  if (line.from == null) return `<div class="line note">${esc(line.text ?? '')}</div>`;
  const you = line.from === 'user', member = line.bot ?? `${sw.project}.${line.from}`;
  // A swarm of more than one kind keeps a wider name column, so its posts still line up.
  const kind = !you && kindOf(sw, member), w = sw.mix.length > 1 ? ' wide' : '';
  const who = you ? `<span class="who you${w}">you</span>` : ['council', 'budget', 'swarm'].includes(line.from) ? `<span class="who council${w}">${esc(line.from)}</span>` : `<button type="button" class="who${w}" data-task="${esc(member)}">${esc(line.from)}${kind ? ` <span class="kind">${esc(kind)}</span>` : ''}</button>`;
  const text = inline(String(line.text ?? '')).replace(/(^|[\s(])@([A-Za-z0-9_.-]*[A-Za-z0-9_-])/g, '$1<span class="at">@$2</span>');
  const row = (cls, body) => `<div class="line post ${cls}">${who}<span class="pt">${body}</span></div>`;
  switch (line.kind) {
    case 'role': return row('ev', `is now <i>${esc(line.role ?? '')}</i>`);
    case 'assign': case 'claim': case 'submit': case 'review': case 'finish': case 'leave': return row('ev', `${line.stream ? streamTag(line.stream) + ' ' : ''}${line.outcome ? esc(line.outcome) + ': ' : ''}${text}`);
    case 'join': return row('ev', `joined ${streamTag(line.stream ?? '')}`);
    case 'vote': return row('ev', `votes <b>${line.yes ? 'yes' : 'no'}</b> on ${esc(line.id ?? '')}${line.text ? `: ${text}` : ''}`);
    case 'seat': return row('ev', `${esc(line.was ?? '')} left · ${esc(line.seat ?? '')} holds a council seat`);
    case 'lead': return row('ev', `${streamTag(line.stream ?? '')} ${esc(line.was ?? '')} left · ${esc(line.lead ?? '')} leads it`);
    case 'decision': return row(`ev decided ${line.approved ? 'yes' : 'no'}`, `${esc(line.id ?? '')} ${streamTag(line.stream ?? '')} ${line.approved ? `approved · ${esc(line.lead ?? '')} leads it` : 'denied'}`);
    case 'propose': {
      const p = sw.state.proposals.find((x) => x.id === line.id);
      return row('prop', `proposes <b>${esc(line.id ?? '')}</b> ${streamTag(line.stream ?? '')}: ${text}${p ? ` <span class="tally">${p.status === 'open' ? tally(sw, p) : p.status}</span>` : ''}`);
    }
    default: return row(you ? 'mine' : '', `${line.stream ? `${streamTag(line.stream)} ` : ''}${text}`);
  }
}
// The council: its seats, each open proposal with the seats' votes and your Approve and Deny, then
// the ones decided.
function councilHTML(sw) {
  const who = (short) => `<button type="button" class="who" data-task="${esc(`${sw.project}.${short}`)}">${esc(short)}</button>`;
  const seats = sw.seats.map((m) => memberShort(sw, m));
  // A majority of the council's size, however many seats are filled now.
  const need = Math.floor(sw.council / 2) + 1;
  const card = (p) => {
    // Once decided, only the votes cast.
    const votes = seats.filter((seat) => p.status === 'open' || p.votes?.[seat]).map((seat) => { const v = p.votes?.[seat]; return `<div class="vote">${who(seat)} ${v ? `<b class="${v.yes ? 'yes' : 'no'}">${v.yes ? 'yes' : 'no'}</b> ${esc(v.reason)}` : '<span class="dim">not yet</span>'}</div>`; }).join('');
    const yours = p.votes?.user ? `<div class="vote"><span class="who you">you</span> <b class="${p.votes.user.yes ? 'yes' : 'no'}">${p.votes.user.yes ? 'approved' : 'denied'}</b></div>` : '';
    const acts = p.status === 'open' ? `<div class="acts"><button type="button" class="sbtn primary" data-act="swarm-decide" data-v="${esc(p.id)}:yes">Approve</button><button type="button" class="sbtn" data-act="swarm-decide" data-v="${esc(p.id)}:no">Deny</button></div>` : '';
    const state = p.status === 'open' ? tally(sw, p) : `${p.status}${p.decided_by === 'user' ? ' by you' : ''}`;
    return `<div class="prop ${esc(p.status)}"><div class="ph"><b>${esc(p.id)}</b> ${streamTag(p.stream)} <span class="dim">by</span> ${who(p.by)}<span class="tally">${esc(state)}</span></div><div class="why">${inline(p.why)}</div>${votes}${yours}${acts}</div>`;
  };
  const open = sw.state.proposals.filter((p) => p.status === 'open'), done = sw.state.proposals.filter((p) => p.status !== 'open').reverse();
  return `<div class="line note">Seats: ${seats.map(esc).join(', ') || 'none yet'}. ${need} of ${sw.council} decide; you can decide any proposal yourself.</div>${open.length ? open.map(card).join('') : '<div class="line note">no open proposals</div>'}${done.map(card).join('')}`;
}
// Streams: each approved proposal, its lead and the agents in it with their roles.
function streamsHTML(sw) {
  // Assigned tasks, then streams a plain proposal opened, then the final result.
  const tasks = Object.entries(sw.state.tasks ?? {});
  const result = sw.state.result;
  const handoff = result ? `<div class="prop"><b>Final result · ${esc(result.outcome)}</b><div class="why">${inline(result.summary ?? '')}</div></div>` : '';
  const work = tasks.map(([name,t]) => `<div class="prop stream"><div class="ph">${streamTag(name)} <b>${esc(t.status)}</b></div><div class="why">${esc(t.owner)} · reviewer ${esc(t.reviewer)}<br>${inline(t.brief ?? '')}</div>${t.result ? `<div class="why">${inline(t.result)}</div>` : ''}${t.verdict ? `<div class="why"><b>${esc(t.verdict)}</b>: ${inline(t.evidence ?? '')}</div>` : ''}</div>`).join('');
  const approved = sw.state.proposals.filter((p) => p.status === 'approved' && !sw.state.tasks?.[p.stream]);
  if (!tasks.length && !approved.length && !result) return sw.council ? '<div class="line note">no streams yet: an approved proposal opens one</div>' : '<div class="line note">no tasks yet: an agent registers one with assign</div>';
  return work + approved.map((p) => {
    const members = Object.entries(sw.state.streams ?? {}).filter(([, st]) => st === p.stream).map(([m]) => m);
    const people = members.map((m) => `<button type="button" class="member" data-task="${esc(`${sw.project}.${m}`)}">${esc(m)}${m === (p.lead ?? p.by) ? ' <span class="dim">lead</span>' : ''}${sw.state.roles?.[m] ? ` <i>${esc(sw.state.roles[m])}</i>` : ''}</button>`).join('');
    return `<div class="prop stream"><div class="ph">${streamTag(p.stream)} <span class="dim">${members.length} agent${members.length === 1 ? '' : 's'}</span><button type="button" class="sbtn" data-act="swarm-filter" data-v="${esc(p.stream)}">Posts</button></div><div class="why">${inline(p.why)}</div><div class="members">${people}</div></div>`;
  }).join('') + handoff;
}
function renderSwarm(el, sw) {
  // Members and seats are in the key: one leaving changes the council and agents views, not the board.
  const key = `${SWARM}${sw.name}|${sw.tab}|${sw.offset}|${sw.lines.length}|${sw.filter}|${sw.stateGen}|${sw.members.join(',')}|${sw.seats.join(',')}`;
  const fresh = el.dataset.who !== swarmKey(sw.name); el.dataset.who = swarmKey(sw.name);
  const atBottom = fresh || el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  if (el.dataset.key !== key) {
    el.dataset.key = key;
    if (sw.tab === 'agents') {
      // Each card says what its agent is, what it took on, and the stream it works in.
      const cards = sw.members.filter((m) => memberBot(sw, m)).map((m) => { const c = taskCard(m), short = memberShort(sw, m); if (c) c.name = [c.name, kindOf(sw, m), sw.state.roles?.[short], sw.state.streams?.[short] && `#${sw.state.streams[short]}`].filter(Boolean).join(' · '); return c; }).filter(Boolean);
      el.innerHTML = cards.length ? cards.map(cardHTML).join('') : '<div class="line note">no agents yet</div>';
    } else if (sw.tab === 'council') el.innerHTML = councilHTML(sw);
    else if (sw.tab === 'streams') el.innerHTML = streamsHTML(sw);
    else {
      const lines = sw.filter ? sw.lines.filter((l) => l.stream === sw.filter) : sw.lines;
      el.innerHTML = lines.length ? lines.map((l) => postHTML(sw, l)).join('') : `<div class="line note">${sw.offset === null ? 'reading the board' : sw.filter ? `nothing on #${esc(sw.filter)} yet` : 'nothing posted yet'}</div>`;
    }
  }
  if (sw.tab === 'agents') refreshLive(el);
  if (atBottom && sw.tab === 'board') el.scrollTop = el.scrollHeight;
}

// ---------- the new swarm sheet ----------
// A goal, how many agents, what they are, where they work, and a budget they share. What they are is
// a mix: rows of an identity (a profile the folder offers, or a plain agent), a model and its effort, and a share,
// shown as the whole agents it makes at the size picked.
// A swarm starts with up to 64 agents, as the app's side takes; Add goes on from there.
const MAX_AGENTS = 64, MAX_BUDGET_M = 1000, BUDGET_PER_AGENT_M = 10;
const MIX_ROWS = 8;
let sheetFor = null;
const sheet = { models: [], profiles: [], mix: [] };
async function openSwarmSheet(project) {
  closeMenu();
  const lead = bot(project + LEAD); if (!lead) return;
  // Its agents start on the project's model and effort unless others are picked.
  let models = []; try { models = connected(await Daemon.models(), setupState().settings ?? await loadSettings().catch(() => null)); } catch (_) {}
  let profiles = []; try { profiles = await Daemon.profiles(lead.workspace); } catch (e) { toast(`profiles: ${e?.message ?? e}`, 5000); }
  const first = [lead.model, lastModel()].find((m) => m && models.some((x) => x.id === m)) ?? '';
  const effort = first === lead.model ? lead.reasoning ?? '' : '';
  Object.assign(sheet, { models, profiles, mix: [{ identity: '', model: first, reasoning: effort, share: 100 }], budgetEdited: false });
  sheetFor = project;
  const sel = (id, opts, on) => `<select id="${id}">${opts.map(([v, l]) => `<option value="${esc(v)}"${String(v) === String(on) ? ' selected' : ''}>${esc(l)}</option>`).join('')}</select>`;
  $('sheet').innerHTML = `<h4>New swarm in ${esc(project)}</h4>
    <label for="sw-goal">Goal</label><textarea id="sw-goal" rows="3" placeholder="What should they get done together?"></textarea>
    <div class="row"><div><label for="sw-n">Agents</label><input id="sw-n" type="number" min="1" max="${MAX_AGENTS}" step="1" value="4"></div><div class="wide"><label for="sw-where">They work in</label>${sel('sw-where', [['shared', 'One shared worktree'], ['project', 'The project folder']], 'shared')}</div><div><label for="sw-budget">Budget (M)</label><input id="sw-budget" type="number" min="0.1" max="${MAX_BUDGET_M}" step="any" value="${4 * BUDGET_PER_AGENT_M}" aria-label="Total budget in millions of tokens"></div></div>
    <div id="sw-each" class="hint"></div>
    <label>Made of <span class="dim">identity, model, effort and share</span></label><div id="sw-mix" class="mix"></div>
    <label for="sw-org">Organized as</label>${sel('sw-org', [[0, 'One board: assigned tasks and independent review'], [3, 'A council of 3 approves streams of work']], 0)}
    <div class="foot"><button type="button" class="sbtn" data-act="close-sheet">Cancel</button><button type="submit" class="sbtn primary" id="sw-start">Start swarm</button></div>`;
  $('sw-budget').value = String(4 * BUDGET_PER_AGENT_M);
  renderMix();
  $('sheetwrap').classList.add('on'); S.ui.sheet = true;
  setTimeout(() => $('sw-goal').focus?.(), 0);
}
// Any whole number of agents typed; the counts show nothing until it is one.
function agentCount() { const n = Number($('sw-n')?.value); return Number.isInteger(n) && n >= 1 && n <= MAX_AGENTS ? n : 0; }
// The budget, typed in millions of tokens; 0 until it is a number in range.
function budgetTokens() { const m = Number($('sw-budget')?.value); return m >= 0.1 && m <= MAX_BUDGET_M ? Math.round(m * 1e6) : 0; }
const sheetProblem = (n) => (!n ? `Agents is a whole number from 1 to ${MAX_AGENTS}` : !budgetTokens() ? `Budget is 0.1 to ${MAX_BUDGET_M} million tokens` : Number($('sw-org')?.value) > n ? 'A council of 3 needs at least 3 agents' : mixProblem(sheet.mix));
// The mix's rows, each with how many agents it makes, and what is wrong with it if anything.
function mixProblem(mix) {
  const total = mix.reduce((a, r) => a + (Number(r.share) || 0), 0);
  if (mix.some((r) => !r.model)) return 'Choose a model for every row';
  if (mix.some((r) => !(r.share >= 1 && r.share <= 100))) return 'Each share is 1 to 100%';
  if (total !== 100) return `The shares add up to ${total}%, not 100%`;
  return null;
}
// Make the total allowance and repeated input accounting clear before launch.
function renderEach(n) {
  const el = $('sw-each'); if (!el) return;
  const each = n ? Math.floor(budgetTokens() / n) : 0;
  el.textContent = n && each ? `${tokens(each)} tokens per agent · includes cached input on every call · ${tokens(Math.floor(each * .2))} each reserved in the work plan for review and reporting${each < 10e6 ? ' · below the recommended 10M per agent for repository work' : ''}` : '';
  el.classList.toggle('warn', !!each && each < 10e6);
}
function updateSwarmBudget(id) {
  if (id === 'sw-budget') sheet.budgetEdited = true;
  if (id === 'sw-n' && !sheet.budgetEdited && agentCount()) $('sw-budget').value = String(agentCount() * BUDGET_PER_AGENT_M);
}
function renderMix() {
  const n = agentCount(), problem = sheetProblem(n);
  renderEach(n);
  const counts = problem ? null : mixCounts(sheet.mix, mixRows(sheet.mix, n));
  const identities = [['', 'Plain agent'], ...sheet.profiles.map((p) => [p.name, p.name])];
  const row = (r, i) => `<div class="mixrow"><select data-mix="${i}" data-f="identity" aria-label="Identity">${identities.map(([v, l]) => `<option value="${esc(v)}"${v === r.identity ? ' selected' : ''}>${esc(l)}</option>`).join('')}</select>${modelSelectHTML(`sw-model-${i}`, sheet.models, r.model).replace('<select ', `<select data-mix="${i}" data-f="model" `)}${effortSelectHTML(`sw-effort-${i}`, r.model, r.reasoning, true).replace('<select ', `<select data-mix="${i}" data-f="reasoning" `)}<span class="share"><input type="number" min="1" max="100" step="1" value="${esc(r.share)}" data-mix="${i}" data-f="share" aria-label="Share in percent">%</span><span class="count${counts && !counts[i] ? ' none' : ''}">${counts ? `${counts[i]} agent${counts[i] === 1 ? '' : 's'}` : ''}</span>${sheet.mix.length > 1 ? `<button type="button" class="x" data-act="mix-remove" data-v="${i}" title="Remove this row">×</button>` : '<span class="x"></span>'}</div>`;
  const add = sheet.mix.length < MIX_ROWS ? '<button type="button" class="sbtn" data-act="mix-add">Add a row</button>' : '';
  const note = problem ?? (counts.some((c) => !c) ? `A row makes no agent at ${n} agents` : '');
  $('sw-mix').innerHTML = `${sheet.mix.map(row).join('')}<div class="mixfoot">${add}<span class="${problem || note ? 'warn' : ''}">${esc(note)}</span></div>`;
}
// A new row takes half the largest row's share; a removed row's share goes to the first row left.
function mixAdd() {
  const big = sheet.mix.reduce((b, r, i) => (r.share > sheet.mix[b].share ? i : b), 0), half = Math.floor(sheet.mix[big].share / 2);
  sheet.mix[big].share -= half;
  sheet.mix.push({ identity: '', model: sheet.mix[big].model, reasoning: sheet.mix[big].reasoning, share: half });
  renderMix();
}
function mixRemove(i) {
  const [gone] = sheet.mix.splice(i, 1); sheet.mix[0].share += gone.share;
  renderMix();
}
// Picking an identity picks the model its profile names, when that model is connected. A row keeps its
// effort while its model takes that level.
function mixChange(el) {
  const i = Number(el.dataset.mix), r = sheet.mix[i]; if (!r) return;
  if (el.dataset.f === 'share') r.share = Math.round(Number(el.value));
  else r[el.dataset.f] = el.value;
  if (el.dataset.f === 'identity') { const m = sheet.profiles.find((p) => p.name === el.value)?.model; if (m && sheet.models.some((x) => x.id === m)) r.model = m; }
  if (r.reasoning && !effortsFor(r.model).includes(r.reasoning)) r.reasoning = '';
  if (el.dataset.f !== 'share' || el.type !== 'number') renderMix();
}
function closeSheet() { if (!S.ui.sheet) return; S.ui.sheet = false; sheetFor = null; $('sheetwrap').classList.remove('on'); markSeen(); focusInput('main'); }
$('sheet').addEventListener('change', (e) => { if (e.target.dataset?.mix != null) mixChange(e.target); else if (e.target.id === 'sw-n' || e.target.id === 'sw-budget' || e.target.id === 'sw-org') { updateSwarmBudget(e.target.id); renderMix(); } });
// A share typed updates the counts once it is a number, without redrawing the field being typed in.
$('sheet').addEventListener('input', (e) => { if (e.target.id === 'sw-n' || e.target.id === 'sw-budget') { updateSwarmBudget(e.target.id); renderMix(); return; } if (e.target.dataset?.f !== 'share') return; mixChange(e.target); const n = agentCount(), problem = sheetProblem(n), counts = problem ? null : mixCounts(sheet.mix, mixRows(sheet.mix, n)); $('sw-mix').querySelectorAll('.count').forEach((c, i) => { c.textContent = counts ? `${counts[i]} agent${counts[i] === 1 ? '' : 's'}` : ''; c.classList.toggle('none', !!counts && !counts[i]); }); const w = $('sw-mix').querySelector('.mixfoot span'); if (w) { w.textContent = problem ?? (counts.some((c) => !c) ? `A row makes no agent at ${n} agents` : ''); w.className = w.textContent ? 'warn' : ''; } });
$('sheet').addEventListener('submit', async (e) => {
  e.preventDefault();
  const project = sheetFor, start = $('sw-start'); if (!project || start.disabled) return;
  start.disabled = true; start.textContent = 'Starting…';
  try {
    const n = agentCount(), problem = sheetProblem(n); if (problem) throw new Error(problem);
    const mix = sheet.mix.map((r) => ({ identity: r.identity, model: r.model, reasoning: r.reasoning || null, share: r.share }));
    await createSwarm(project, { goal: $('sw-goal').value, n, mix, shared: $('sw-where').value === 'shared', budget: budgetTokens(), council: Number($('sw-org').value) });
    closeSheet();
  } catch (err) { toast(String(err?.message ?? err), 6000); start.disabled = false; start.textContent = 'Start swarm'; }
});
$('sheet').addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); $('sheet').requestSubmit(); } });

// ---------- sidebar ----------
// The sidebar shows a window of rows around the selection; scrolling to an edge extends it. The rows
// are rebuilt when the fleet's shape changes (a bot created, forked or deleted, a project folded); a
// status change patches the bot's own row. A fleet of thousands costs a screenful of rows, not a row
// each per event.
const RAIL_ROWS = 300;
const rail = { shapeGen: -1, drawn: -1, selected: null, rows: [], index: new Map(), start: 0, end: 0, key: '' };
function railRows() {
  if (rail.shapeGen !== S.shapeGen) {
    rail.rows = tree(); rail.index = new Map(); rail.rows.forEach((n, i) => { const k = n.key ?? n.b?.name; if (k) rail.index.set(k, i); });
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
    el.innerHTML = above + rows.slice(rail.start, rail.end).map((n) => botRowHTML(n, (n.key ?? n.b?.name) === S.selected)).join('') + below;
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
  if (n.swarm) {
    const sw = n.swarm, st = swarmStatus(sw);
    return `<div class="botrow${sel ? ' sel' : ''}" data-bot="${esc(n.key)}" role="button" tabindex="0"><span class="tree">${n.prefix}</span><span class="glyph ${st}">${glyphOf(st)}</span><span class="n">⁂ ${esc(memberShort(sw, sw.name))}</span><span class="meta">${sw.members.length}</span><span class="acts">${moreButton(n.key)}</span></div>`;
  }
  const b = n.b, acts = `<span class="acts">${moreButton(b.name)}</span>`;
  const st = shownStatus(b), glyph = `<span class="glyph ${st}">${glyphOf(st)}</span>`;
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
  const b = bot(S.selected), sw = swarmOf(S.selected);
  if (S.ui.side && (!S.bots.has(S.ui.side) || S.ui.side === S.selected)) S.ui.side = null;
  const side = S.ui.side && !S.ui.file ? bot(S.ui.side) : null;
  app.classList.toggle('rail', S.ui.rail); app.classList.toggle('side', !!side || !!S.ui.file);
  followDrafts();
  markSeen();
  if (sw) { renderSwarmHead($('title'), sw); renderSwarm($('log'), sw); }
  else { renderHead($('title'), b, 'main'); if (b) renderTranscript($('log'), b.name); else { $('log').innerHTML = ''; $('log').dataset.key = ''; } }
  if (S.ui.rail) renderRail();
  if (S.ui.file) renderFile();
  else if (side) { renderHead($('sidetitle'), side, 'side'); renderTranscript($('side'), side.name); }
  $('sideform').hidden = !!S.ui.file;
  renderComposer('main', b, sw); renderComposer('side', side);
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
    const st = shownStatus(r.b), state = st === 'idle' ? '' : labelOf(st);
    const hint = q ? [creatorOf(r.b) ? `↳ ${r.b.parent}` : '', state].filter(Boolean).join(' · ') : state;
    return `<div class="row${idx === S.ui.pickerSel ? ' sel' : ''}" data-pick="${esc(n)}">${q ? '' : `<span class="tree">${r.prefix}</span>`}<span class="glyph ${st}">${glyphOf(st)}</span><span class="n">${hit}</span><span class="h">${esc(hint)}</span></div>`;
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
  const shown = S.ui.help = {}; const text = (models) => { $('helptext').innerHTML = `<b>keys</b>\n ^k   find a bot        ^b   sidebar\n ^p   next task beside  Esc  close beside · stop\n ^o   all steps         ^d   detach (close)\n ↑ ↓  previous / next bot   ^,   settings\n Enter sends · Shift-Enter a new line\n\n /new NAME PROVIDER/MODEL [EFFORT]  create a bot\n${models}\n<i>any key closes this</i>`; };
  text('   reading ~/.agent/models'); $('helpwrap').classList.add('on');
  let models; try { const list = await Daemon.models(); models = list.length ? list.map((m) => `   ${esc(m.id)}`).join('\n') : '   none listed: Settings lists your providers\' models'; } catch (e) { models = `   ${esc(String(e?.message ?? e))}`; }
  if (S.ui.help === shown) text(models);
}
function hideHelp() { S.ui.help = false; $('helpwrap').classList.remove('on'); markSeen(); $(PANE[S.ui.side ? helpPane : 'main'].input).focus(); }

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
  if (!isOpen(menuFor)) { closeMenu(); return; }
  const items = botMenuItems(menuFor);
  if (menuSig(items) !== menuKey) showMenu(items, menuAnchor, menuFor);
}
// Keep, which turns a side chat into a task, is not built yet.
function botMenuItems(name) {
  const sw = swarmOf(name);
  if (sw) return [
    { act: 'swarm-stop', who: name, label: 'Stop every agent', disabled: sw.stopped && !sw.members.some((m) => memberBot(sw, m)?.runningTurn != null) },
    // A stopped swarm takes no new agent until your next post resumes it.
    { act: 'swarm-add', who: name, label: 'Add an agent', disabled: sw.stopped, hint: sw.stopped ? 'post to resume' : '' },
  ];
  const b = bot(name); if (!b) return [];
  const busy = isActive(b.status);
  return [
    // A swarm runs on this machine: its board is the app's files and its agents run the app's scripts.
    ...(leadProject(name) ? [{ act: 'new-swarm', who: name, label: 'New swarm', hint: S.config?.host ? 'local only' : '⁂', disabled: !!S.config?.host }, { sep: true }] : []),
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
  // Effort first: a few fixed levels above a list that may scroll. An agent made with a level
  // always sends one; one made without may go back to the model's own.
  const effort = effortOf(b);
  const items = [{ head: 'Effort' }];
  if (!b.reasoning) items.push({ act: 'set-effort', who: b.name, v: '', label: 'default', on: !effort });
  for (const level of effortsFor(b.model)) items.push({ act: 'set-effort', who: b.name, v: level, label: level, on: level === effort });
  items.push({ sep: true });
  // Each provider under its own heading, so the menu says where a model comes from.
  let group = null;
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
// A level for the agent's next turns; its own level, or none when it has none, clears the pick.
function setEffort(name, level) {
  const b = bot(name); if (!b) return false;
  if (!level || level === b.reasoning) S.effort.delete(name);
  else if (effortsFor(b.model).includes(level)) S.effort.set(name, level);
  else return false;
  save(); return true;
}
async function modelMenu(pane, anchor) {
  const b = bot(PANE[pane].bot()); if (!b) return;
  // Read now, so an edited ~/.agent/models shows without a restart.
  let list = [], error = null;
  try {
    // A list written before a removal may still name providers this daemon does not run.
    const set = setupState().settings ?? await loadSettings().catch(() => null);
    list = connected(await Daemon.models(), set); if (!list.length) error = 'Settings lists your providers\' models';
  } catch (e) { error = String(e?.message ?? e); }
  // The pane may show another bot, or this name another identity, by the time the list is read.
  if (PANE[pane].bot() !== b.name || bot(b.name) !== b) return;
  showMenu(modelMenuItems(b, list, error), anchor);
}

// ---------- actions ----------
// `to` is the bot the text was typed for, which is the one it goes to even if the pane has since
// been pointed at another.
async function submit(text, pane = 'main', to = PANE[pane].bot()) {
  if (pane === 'main' && text.startsWith('/new ')) {
    const [name, model, effort] = text.slice(5).trim().split(/\s+/);
    if (!name) throw new Error('name_required');
    const m = model; if (!m) throw new Error('model_required: /new NAME PROVIDER/MODEL [EFFORT]');
    // The daemon judges the level against the model's family.
    // Composed now, so an AGENTS.md edited since the window opened reaches this bot. One that
    // cannot be composed rejects here and nothing is created, as with the CLI's --agents.
    const policy = await Daemon.policy();
    const session = S.session;
    const record = await Daemon.request('create', { bot: name, workspace: S.config.workspace, model: m, ...(effort ? { reasoning: effort } : {}), instructions: policy.instructions, compaction_instructions: policy.compaction_instructions, tools: S.config.tools });
    await enqueue(() => { if (S.session === session) seat(record, session); });
    await openOnly(name); toast(`created ${name} · ${policy.note}`); return;
  }
  if (text === '/help' || text === '?') { showHelp(pane); return; }
  const sw = pane === 'main' && swarmOf(to);
  if (sw) { await postToSwarm(sw, text); return; }
  const b = bot(to); if (!b) throw new Error('no bot selected; /new NAME PROVIDER/MODEL [EFFORT] creates one');
  // An event can seat a bot before its snapshot identity arrives. Never send an unpinned name.
  if (b.id == null) throw new Error('bot_identity_pending: wait for attachment to finish');
  const mode = sendMode(b), model = S.override.get(b.name), effort = S.effort.get(b.name), delivery = mode === 'send' ? 'reject' : mode;
  if (mode === 'side') { await sideChat(b.name, text); return; }
  // A steer joins the running turn only on that turn's model, effort and folder, so it names none.
  // It also names the turn on screen, so a turn that ended meanwhile refuses it as stale_turn
  // rather than the message landing in whatever turn runs next. A bot keeps its folder, so a
  // message names one only for a bot that has none.
  const where = delivery === 'steer' ? (b.runningTurn != null ? { expected_turn: b.runningTurn } : {}) : { ...home(b), ...(model && model !== b.model ? { model } : {}), ...(effort && effort !== b.reasoning ? { reasoning: effort } : {}) };
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
async function createProject(dir, picked = null, effort = null) {
  const info = await Daemon.project(dir);
  const existing = bot(info.coordinator);
  if (existing) {
    if (existing.workspace !== info.dir) throw new Error(`${info.coordinator} already belongs to ${existing.workspace ?? 'another folder'}`);
    if (!info.file) await Daemon.writeProject({ dir: info.dir, name: info.name, model: existing.model, reasoning: existing.reasoning ?? null });
    await openOnly(info.coordinator); return;
  }
  const policy = await Daemon.policy(info.dir, 'coordinator');
  // A folder that already has a project file keeps its model and effort; a new one takes the model
  // picked for it, else its coordinator profile's, and the effort picked beside it.
  const kept = !!(info.file && info.model);
  const model = kept ? info.model : picked || policy.model;
  if (!model) throw new Error('model_required: choose a model');
  const reasoning = (kept ? info.reasoning : effort) || null;
  if (picked) try { localStorage.setItem('agent:model', picked); } catch (_) {}
  if (effort !== null) try { localStorage.setItem('agent:effort', effort); } catch (_) {}
  const session = S.session;
  const record = await Daemon.request('create', { bot: info.coordinator, workspace: info.dir, model, ...(reasoning ? { reasoning } : {}), instructions: policy.instructions, compaction_instructions: policy.compaction_instructions, tools: policy.tools ?? S.config.tools });
  await enqueue(() => { if (S.session === session) seat(record, session); });
  if (!info.file) await Daemon.writeProject({ dir: info.dir, name: info.name, model, reasoning });
  await openOnly(info.coordinator); toast(`project ${info.name} · ${policy.note}`);
}
function detach() { save(); Daemon.close(); }

// ---------- setup ----------
// What a daemon needs before anything runs: providers, and what each needs to sign in. The app keeps
// them in `~/.agent/env` (read back without key values) and restarts the daemon to apply them. There
// is no default model: each project or agent is given one when it is made, from any provider.
// Onboarding is this screen opened on its own when no provider is set up; Settings is the same
// screen opened from the sidebar.
const AWS = 'Signs in with your AWS CLI (version 2) login for the profile (aws configure, or aws sso login), or with a Bedrock API key.';
// A field that is `local` is the form's own choice, never saved.
const BEDROCK = [
  { key: 'AWS_REGION', label: 'Region', hint: 'us-east-1', required: true },
  { key: 'AUTH', label: 'Sign in with', local: true, choices: [['aws', 'AWS login'], ['key', 'Bedrock API key']] },
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
// A spec the catalog writes, which its form can edit; any other (a gateway under a known name) it
// would overwrite with the provider's defaults.
const editable = (spec) => spec === specName(spec) ? !!catalogOf(spec) : !!catalogOf(specName(spec))?.parts
  && providerSpecs(catalogOf(specName(spec)).id, { AWS_REGION: spec.split('.')[1], AWS_BEARER_TOKEN_BEDROCK: 'x' }).includes(spec);
// Effort: how hard a model thinks, picked beside its model when an agent is made; the model chip
// changes it for the agent's next turns. Both families take low to xhigh and Anthropic's also max;
// which of those a model accepts is its provider's to say. No level sends none, and the model uses
// its own default.
const EFFORTS = ['low', 'medium', 'high', 'xhigh'];
// The wire family a provider speaks: what the fleet's records show, what a hand-set spec names, else
// the catalog's. Anthropic is the only other family.
function familyOf(provider) {
  if (S.families.has(provider)) return S.families.get(provider);
  const spec = ((S.setup?.settings ?? S.seenSettings)?.providers ?? []).find((s) => specName(s) === provider && String(s).includes('='));
  if (spec) return String(spec).split('=')[1].split(',')[0];
  const part = catalogOf(provider)?.parts?.find(([name]) => name === provider);
  return part ? part[1] : provider === 'anthropic' ? 'anthropic' : 'responses';
}
const effortsFor = (model) => familyOf(providerOf(model)) === 'anthropic' ? [...EFFORTS, 'max'] : EFFORTS;
function lastEffort() { try { return localStorage.getItem('agent:effort') ?? ''; } catch (_) { return ''; } }
// An effort picker for `model`: its levels, `prefer` chosen when the model takes it. Where no label
// names the field, each level says what it is.
function effortSelectHTML(id, model, prefer = lastEffort(), labelled = false) {
  const levels = model ? effortsFor(model) : EFFORTS, pick = levels.includes(prefer) ? prefer : '', word = labelled ? '' : ' effort';
  return `<select id="${id}" aria-label="Effort" title="How hard the model thinks; the model chip changes it later"><option value=""${pick ? '' : ' selected'}>default${word}</option>${levels.map((l) => `<option value="${l}"${l === pick ? ' selected' : ''}>${l}${word}</option>`).join('')}</select>`;
}
// A model picked in a form offers that model's levels, keeping the level chosen when it still applies.
function followModel(model, effortId, labelled = false) { const el = $(effortId); if (el) el.outerHTML = effortSelectHTML(effortId, model, el.value, labelled); }
// A model picker: every listed model under its provider's name, the last one picked chosen.
function lastModel() { try { return localStorage.getItem('agent:model'); } catch (_) { return null; } }
// The model a picker over `list` starts on.
const pickedModel = (list, prefer = null) => [prefer, lastModel()].find((m) => m && list.some((x) => x.id === m)) ?? '';
function modelSelectHTML(id, list, prefer = null) {
  const pick = pickedModel(list, prefer);
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
  // The hosts come as OpenSSH resolves them, beside the rest.
  Daemon.hosts?.().then((hosts) => { st.hosts = hosts; st.hostsError = null; renderSetup(); }, (e) => { st.hosts = []; st.hostsError = String(e?.message ?? e); renderSetup(); });
  try { await loadSettings(); } catch (e) { st.error = String(e?.message ?? e); }
  try { st.roles = await Daemon.roles?.(); } catch (_) {}
  if (!st.settings?.providers?.length) st.adding = st.adding ?? '';
  await readSchedules();
  renderSetup();
  await Promise.all([checkProviders(st.settings?.listing), readList()]);
}
// What this machine would start a daemon with. A window attached to a daemon it did not start shows
// that daemon's providers instead, which are the ones its models come from.
async function loadSettings() {
  const set = await Daemon.settings();
  if (set.restartable === false && S.attached) {
    // The same answer says how each provider is doing, so it is not asked for twice.
    try { set.listing = (await Daemon.request('provider_models', {})).providers ?? {}; set.providers = Object.keys(set.listing); } catch (_) {}
  }
  setupState().settings = set;
  return set;
}
// Only models a connected provider can run: a list written before a removal may still name others.
const connected = (list, set) => set ? list.filter((m) => set.providers.map(specName).includes(providerOf(m.id))) : list;
function closeSetup() {
  const st = S.setup; if (!st || st.busy) return;
  st.open = false; st.adding = null; $('setupwrap').classList.remove('on'); render(); focusInput('main');
}
// Each provider's own answer, read without writing anything: the daemon keeps listings a while.
async function checkProviders(listing) {
  const st = setupState(); const names = (st.settings?.providers ?? []).map(specName);
  if (!S.attached || !names.length) { st.status = {}; renderSetup(); return; }
  st.status = Object.fromEntries(names.map((n) => [n, 'checking'])); renderSetup();
  try { const providers = listing ?? (await Daemon.request('provider_models', {})).providers; st.status = Object.fromEntries(names.map((n) => [n, answerOf(providers?.[n])])); }
  catch (e) { st.status = Object.fromEntries(names.map((n) => [n, { error: String(e?.message ?? e) }])); }
  renderSetup();
}
const answerOf = (listed) => !listed ? { error: 'not running; restart to apply' } : Array.isArray(listed.models) ? { models: listed.models.length } : { error: listed.error ?? 'no listing', detail: listed.detail ?? null };
async function readList() {
  const st = setupState();
  try { st.list = connected(await Daemon.models(), st.settings); st.listError = null; } catch (e) { st.list = []; st.listError = String(e?.message ?? e); }
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
  unrestartable();
  // Read again, so a change another window saved meanwhile is kept.
  await loadSettings();
  // A key already set, saved here or exported by the shell, answers for an empty field.
  for (const f of c.fields) if (f.required && !values[f.key] && !(f.secret && st.settings?.keys?.includes(f.key))) throw new Error(`${f.label} is required`);
  const specs = (st.settings?.providers ?? []).filter((s) => catalogOf(specName(s)) !== c);
  if ((st.settings?.providers ?? []).some((s) => catalogOf(specName(s)) === c && !editable(s))) throw new Error(`${c.label} is set up by hand; remove it to set it up here`);
  // Bedrock signs in one way: with the AWS login, which drops a saved key, or with a key, typed or saved.
  const saved = st.settings?.keys?.includes('AWS_BEARER_TOKEN_BEDROCK');
  const aws = values.AUTH ? values.AUTH === 'aws' : !values.AWS_BEARER_TOKEN_BEDROCK && !saved;
  // The region names the endpoint and sits inside a space-separated provider list.
  if (c.parts && !/^[a-z]{2}(-[a-z]+)+-\d+$/.test(values.AWS_REGION ?? '')) throw new Error(`Region must look like us-east-1, not "${values.AWS_REGION}"`);
  if (c.parts && !aws && !values.AWS_BEARER_TOKEN_BEDROCK && !saved) throw new Error('Bedrock API key is required');
  const keyed = c.parts ? { ...values, AWS_BEARER_TOKEN_BEDROCK: aws ? '' : values.AWS_BEARER_TOKEN_BEDROCK || 'saved' } : values;
  const changes = { AGENT_PROVIDER: [...specs, ...providerSpecs(c.id, keyed)].join(' ') };
  // A key left empty keeps the one saved; another field left empty is cleared, the shell's value too.
  for (const f of c.fields) if (!f.local && (!f.secret || values[f.key])) changes[f.key] = values[f.key] || '';
  // Emptied rather than removed, so a key the shell exports stays out of it too.
  if (c.parts && aws && saved) changes.AWS_BEARER_TOKEN_BEDROCK = '';
  await applySettings(changes);
  st.adding = null;
  await refreshModels();
}
async function removeProvider(name) {
  const st = setupState(); const c = catalogOf(name);
  unrestartable();
  await loadSettings();
  const gone = c ? partsOf(c) : [name];
  const specs = (st.settings?.providers ?? []).filter((s) => !gone.includes(specName(s)));
  // Emptied rather than removed, so a list the shell exports does not come back.
  const changes = { AGENT_PROVIDER: specs.join(' ') };
  // Its key goes too, unless another provider still uses it; the region and profile stay.
  const used = specs.flatMap(keysOf);
  for (const f of c?.fields ?? []) if (f.secret && !used.includes(f.key)) changes[f.key] = null;
  // With no provider named, a start detects one from any key the shell exports; an empty key hides it.
  if (!specs.length) for (const key of DETECTED) if (key in changes || st.settings?.keys?.includes(key)) changes[key] = '';
  await applySettings(changes);
  // The list is not rewritten: the pickers leave its models out, and a hand-added line elsewhere stays.
  await Promise.all([checkProviders(), readList()]);
}
// A window attached through a socket it did not start cannot apply a change, so none is saved.
function unrestartable() {
  const host = setupState().settings?.host;
  if (host) throw new Error(`restart_unavailable: this window's daemon runs on ${host} with the providers its login shell there exports; change them there`);
  if (setupState().settings?.restartable === false) throw new Error('restart_unavailable: this window did not start its daemon, so it cannot apply provider changes');
}
async function applySettings(changes) {
  const st = setupState();
  st.busy = 'Saving…'; st.error = null; renderSetup();
  try { await Daemon.saveSettings(changes); st.settings = await Daemon.settings(); } finally { st.busy = null; }
  await restartDaemon();
}
function setupHTML(kept = new Map()) {
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
  const rows = entries.map(({ key, label, names }) => { const failed = names.some((n) => st.status[n]?.error); return `<div class="prow"><span class="pn">${esc(label)}</span>${statusHTML(names)}<span class="acts">${failed ? `<button type="button" class="sbtn" data-act="setup-retry"${busy}>Retry</button>` : ''}${catalogOf(key) && specs.filter((s) => names.includes(specName(s))).every(editable) ? `<button type="button" class="sbtn" data-act="setup-pick" data-v="${esc(key)}"${busy}>Edit</button>` : ''}<button type="button" class="sbtn${st.confirm === key ? ' danger' : ''}" data-act="setup-remove" data-v="${esc(key)}"${busy}>${st.confirm === key ? 'Remove anyway' : 'Remove'}</button></span>${errorsHTML(names)}${st.confirm === key ? '<div class="perr warn">Agents are working. Removing restarts the daemon, which stops them.</div>' : ''}</div>`; }).join('');
  let add = '';
  if (st.adding === null) add = `<button type="button" class="sbtn" data-act="setup-add"${busy}>＋ Add a provider</button>`;
  else if (st.adding === '') add = `<div class="choices">${CATALOG.filter((c) => !specs.some((s) => catalogOf(specName(s)) === c)).map((c) => `<button type="button" class="choice" data-act="setup-pick" data-v="${c.id}"${busy}>${esc(c.label)}</button>`).join('')}</div>${specs.length ? `<button type="button" class="sbtn" data-act="setup-cancel">Cancel</button>` : ''}`;
  else {
    const c = catalogOf(st.adding);
    const value = (f) => f.key === 'AWS_REGION' ? set?.region ?? '' : f.key === 'AWS_PROFILE' ? set?.profile ?? '' : '';
    // How Bedrock signs in now, from its specs: a key only the shell exports does not change it.
    const choice = (f) => { const on = (set?.providers ?? []).some((s) => catalogOf(specName(s))?.parts && s.endsWith(',AWS_BEARER_TOKEN_BEDROCK')) ? 'key' : 'aws'; return `<label><span>${esc(f.label)}</span><select name="${f.key}">${f.choices.map(([v, l]) => `<option value="${v}"${v === on ? ' selected' : ''}>${esc(l)}</option>`).join('')}</select></label>`; };
    const fields = c.fields.map((f) => f.choices ? choice(f) : `<label><span>${esc(f.label)}</span><input name="${f.key}" type="${f.secret ? 'password' : 'text'}" autocomplete="off" spellcheck="false" value="${esc(value(f))}" placeholder="${esc(f.secret && set?.keys?.includes(f.key) ? 'saved; type to replace' : f.hint ?? '')}"></label>`).join('');
    const working = anyActive() ? `<p class="warn">Agents are working. Connecting restarts the daemon, which stops them.</p>` : '';
    add = `<form class="pform" id="setupform"><b>${esc(c.label)}</b>${c.about ? `<p>${esc(c.about)}</p>` : ''}${fields}${working}<div class="row"><button type="submit" class="sbtn primary"${busy}>Connect</button><button type="button" class="sbtn" data-act="setup-cancel"${busy}>Cancel</button></div></form>`;
  }
  const ready = Object.values(st.status).some((s) => s?.models > 0) || st.list.length > 0;
  const refresh = specs.length && S.attached ? `<button type="button" class="sbtn" data-act="setup-refresh"${busy}>Refresh models</button>` : '';
  const listed = st.listError ? `<p class="bad">${esc(st.listError)}</p>` : '';
  const projects = hasProject();
  const model = pickedModel(st.list, kept.get('setupmodel'));
  const effort = kept.get('setupeffort') ?? lastEffort();
  // Making a project reads its folder, which on a host is the host's.
  const project = projects ? '' : S.config?.host
    ? `<p class="dim">A project on ${esc(S.config.host)} is made there for now: the app does not yet read a host's files (AGENTS.md, profiles, .agents/project.toml). Start its lead there with agent run --new --agents --bot NAME.lead; it shows here.</p>`
    : ready && specs.length && st.list.length
    ? `<form id="setupproj"><label><span>Folder</span><input id="setupdir" autocomplete="off" spellcheck="false" value="${esc(S.config?.workspace ?? '')}"></label><label><span>Model</span>${modelSelectHTML('setupmodel', st.list, model)}</label><label><span>Effort</span>${effortSelectHTML('setupeffort', model, effort, true)}</label><div class="row"><button type="submit" class="sbtn primary"${S.attached ? '' : ' disabled'}${busy}>Create project</button></div><p class="dim">The project's lead runs on this model and effort; every agent you start can use others.</p></form>`
    : `<p class="dim">${specs.length ? 'No models listed yet: see the providers above, then Refresh models.' : 'Connect a provider first.'}</p>`;
  const step = (n, title, done, body) => body ? `<section class="${done ? 'done' : ''}"><h3><span class="num">${done ? '✓' : n}</span>${title}</h3>${body}</section>` : '';
  return `<div class="shead"><b>${ready && projects ? 'Settings' : 'Set up Agent'}</b><button type="button" class="ibtn" data-act="setup-close" title="Close" aria-label="Close"${busy}>✕</button></div>`
    + step(1, 'Providers', ready, `${rows}<div class="row">${set?.host ? `<p class="dim">This window's daemon runs on ${esc(set.host)}, with the providers its login shell there exports; change them there.</p>` : set?.restartable === false ? '<p class="dim">This window uses a daemon it did not start, so it cannot apply provider changes.</p>' : add}${st.adding === null && !set?.host ? refresh : ''}</div>${listed}`)
    + step(2, 'First project', projects, project)
    + (projects && !S.config?.host ? rolesHTML(st, busy) : '')
    + hostsHTML(st, busy)
    + (!S.config?.host && (projects || st.schedules?.length || st.schedulesAfter || st.schedulesError) ? schedulesHTML(st, busy) : '')
    + (st.busy ? `<p class="busy">${esc(st.busy)}</p>` : '') + (st.error ? `<p class="bad">${esc(st.error)}</p>` : '');
}
// The hosts in ~/.ssh/config, each of which a window can be opened on. That window's agents run on
// the host, through the app's own ssh connection to it.
function hostsHTML(st, busy) {
  if (!Daemon.openHost) return '';
  const where = (to) => (to ? `${to.user ? `${to.user}@` : ''}${to.hostname ?? ''}${to.port && to.port !== '22' ? `:${to.port}` : ''}` : '');
  const rows = (st.hosts ?? []).map((h) => `<div class="prow"><span class="pn">${esc(h.alias)}</span><span class="st dim">${esc(where(h.to))}</span><span class="acts"><button type="button" class="sbtn" data-act="open-host" data-v="${esc(h.alias)}"${busy}>Open window</button></span></div>`).join('');
  const none = st.hostsError ? `<p class="bad">${esc(st.hostsError)}</p>` : rows ? '' : '<p class="dim">No hosts in ~/.ssh/config.</p>';
  return `<section><h3>Hosts</h3>${rows}${none}<p class="dim">A window on a host runs its agents there, over ssh with your keys. The host needs its own Linux agent on its login shell's PATH, and its own providers.</p></section>`;
}
// The app's roles, each read from your own file in every project once you have one. Edit makes that
// file from the app's text the first time and opens it in your editor.
const ROLES = [['coordinator', 'Coordinator'], ['swarm-flat', 'Flat swarm'], ['swarm-council', 'Council swarm']];
function rolesHTML(st, busy) {
  const own = new Map((st.roles ?? []).map((r) => [r.name, r.file]));
  const rows = ROLES.map(([name, label]) => `<div class="prow"><span class="pn">${label}</span><span class="st${own.get(name) ? '' : ' dim'}">${own.get(name) ? `~/.agents/agents/${name}.md` : 'the app\'s own'}</span><span class="acts"><button type="button" class="sbtn" data-act="edit-role" data-v="${name}"${busy}>Edit</button></span></div>`).join('');
  return `<section><h3>Roles</h3>${rows}<p class="dim">A project's coordinator and a swarm's agents follow these in every project; a project's own .agents/agents file of that name comes first. An agent keeps the text it started with, so an edit reaches new projects and swarms.</p></section>`;
}
// Agents wake at set times from schedules they or their coordinator made; the Mac keeps the time.
// Each shows who it wakes, when, what its last time did, and the message it sends.
// A schedule that ended on its own without delivering stays listed, saying why, until it is removed; so do a
// one-off still there after its time and a plist that cannot be read.
async function readSchedules(after = null) {
  const st = setupState();
  st.schedulesAfter = after;
  try {
    const page = S.config?.host ? null : await Daemon.schedules?.(after);
    st.schedules = page?.schedules ?? null; st.schedulesNext = page?.next_after ?? null; st.schedulesError = null;
  } catch (e) { st.schedules = null; st.schedulesNext = null; st.schedulesError = String(e?.message ?? e); }
}
const LAST = { sent: 'sent', skipped: 'skipped, it was working', gone: 'its agent is gone', failed: 'failed', missed: 'missed, its time passed long ago' };
function schedulesHTML(st, busy) {
  const at = (ms) => new Date(ms).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
  const last = (l, ended) => l ? `${ended ? 'ended' : 'last'} ${at(l.fired_ms)}: ${LAST[l.outcome] ?? l.outcome}${l.outcome === 'failed' && l.detail ? ` (${String(l.detail).slice(0, 120)})` : ''}` : 'not run yet';
  const remove = (x) => `<span class="acts"><button type="button" class="sbtn" data-act="schedule-remove" data-v="${esc(x.name)}"${busy}>Remove</button></span>`;
  const rows = (st.schedules ?? []).map((x) => x.problem
    ? `<div class="prow"><span class="pn">${esc(x.name)}</span><span class="st bad">unreadable</span>${remove(x)}<div class="sub dim">${esc(x.problem)}</div></div>`
    : `<div class="prow"><span class="pn">${esc(x.bot)}</span><span class="st${x.ended || x.missed ? ' bad' : ''}">${x.ended ? 'not delivered' : x.missed ? 'missed its time' : esc(x.when)}</span>${remove(x)}<div class="sub dim">${esc(last(x.last, x.ended))}${x.name !== x.bot ? ` · ${esc(x.name)}` : ''}</div><div class="sub dim">${esc(x.message.length > 240 ? `${x.message.slice(0, 240)}…` : x.message)}</div></div>`).join('');
  const none = st.schedulesError ? `<p class="bad">${esc(st.schedulesError)}</p>` : rows ? '' : '<p class="dim">None yet. Ask a coordinator, for example "have build check its PR every 30 minutes".</p>';
  return `<section><h3>Schedules</h3>${rows}${none}${st.schedulesAfter ? '<button class="sbtn" data-act="schedules-first">First page</button>' : ''}${st.schedulesNext ? '<button class="sbtn" data-act="schedules-next">Next page</button>' : ''}<p class="dim">Each time, the agent gets its message in its own chat. A repeating one skips a time its agent is working; a one-off waits for it. They run with the app closed; a time the Mac slept through runs once when it wakes.</p></section>`;
}
async function removeSchedule(name) {
  const st = setupState();
  try { await Daemon.removeSchedule(name); } catch (e) { toast(`remove ${name}: ${e?.message ?? e}`, 5000); }
  await readSchedules(st.schedulesAfter);
  renderSetup();
}
async function editRole(name) {
  const st = setupState();
  try { await Daemon.editRole(name); } catch (e) { toast(`edit ${name}: ${e?.message ?? e}`, 5000); }
  try { st.roles = await Daemon.roles(); } catch (_) {}
  renderSetup();
}
// Answers arrive while someone types a key: what the fields hold, and where the caret is, survive.
function renderSetup() {
  if (!S.setup?.open) return;
  const box = $('setup'), kept = new Map();
  for (const el of box.querySelectorAll('input, select')) kept.set(el.name || el.id, el.value);
  const focused = box.contains?.(document.activeElement) ? document.activeElement.name || document.activeElement.id : null;
  box.innerHTML = setupHTML(kept);
  for (const el of box.querySelectorAll('input, select')) { const v = kept.get(el.name || el.id); if (v !== undefined) el.value = v; if (focused && (el.name || el.id) === focused) el.focus(); }
}

// ---------- opening threads ----------
// From the sidebar or the finder a thread takes the whole window, with nothing beside it.
async function openOnly(name) {
  if (!isOpen(name)) return;
  S.selected = name; S.autoSelect = false; S.ui.side = null;
  const sw = swarmOf(name);
  const p = (sw ?? bot(name)).project; if (p && S.ui.folded.delete(p)) S.shapeGen += 1;
  if (sw) { readBoard(sw); readUsage(sw); }
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
  // A swarm has no place beside, so its agent takes the window alone.
  if (swarmOf(S.selected)) { openOnly(S.ui.side); return; }
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
  Promise.all([Daemon.models(), S.setup?.settings ?? Daemon.settings?.().catch(() => null)]).then(([all, set]) => { if (set) S.seenSettings = set; const list = connected(all, set?.restartable === false && !S.setup?.settings ? null : set); if (!$('projform').hidden) $('projmodel').innerHTML = list.length ? modelSelectHTML('projsel', list) + effortSelectHTML('projeffort', pickedModel(list)) : '<span class="dim">no models listed: see Settings</span>'; }, (e) => { $('projmodel').textContent = String(e?.message ?? e); });
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
  try { await createProject(dir, $('projsel')?.value || null, $('projeffort')?.value ?? null); showNewProject(false); focusInput('main'); } catch (err) { toast(String(err?.message ?? err), 5000); }
});
$('projform').addEventListener('change', (e) => { if (e.target.id === 'projsel') followModel(e.target.value, 'projeffort'); });
$('projform').addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.target.id === 'projsel' || e.target.id === 'projeffort')) { e.preventDefault(); $('projform').requestSubmit(); } else if (e.key === 'Escape') { showNewProject(false); focusInput('main'); e.preventDefault(); e.stopPropagation(); } });
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
  if (S.ui.sheet) { if (e.key === 'Escape') { closeSheet(); e.preventDefault(); } return; }
  if (S.setup?.open) { if (e.key === 'Escape') { closeSetup(); e.preventDefault(); } return; }
  if ((e.ctrlKey || e.metaKey) && e.key === ',') { await openSetup(); e.preventDefault(); return; }
  // The finder and the folder field handle their own keys; Escape there must not stop a turn.
  if (S.ui.picker || e.target.id === 'projdir' || e.target.id === 'projsel' || e.target.id === 'projeffort' || e.target.id === 'pickerq') return;
  const k = e.key, ctrl = e.ctrlKey || e.metaKey;
  if (S.ui.menu) { if (k === 'Escape') { closeMenu(); e.preventDefault(); } return; }
  if (ctrl && k === 'k') { openPicker(); e.preventDefault(); return; }
  // A hidden sidebar patches no rows, so it draws them all again when it opens.
  if (ctrl && k === 'b') { S.ui.rail = !S.ui.rail; if (S.ui.rail) rail.key = ''; render(); save(); e.preventDefault(); return; }
  if (ctrl && k === 'd') { detach(); e.preventDefault(); return; }
  if (ctrl && k === 'o') { S.ui.steps = !S.ui.steps; render(); save(); e.preventDefault(); return; }
  if (ctrl && k === 'p') { await nextBeside(); e.preventDefault(); return; }
  if (k === 'Escape') { if (S.ui.file) closeFile(); else if (S.ui.side) closeSide(); else await interrupt(); e.preventDefault(); return; }
  const empty = e.target.id === 'input' && $('input').value === '';
  if (empty && (k === 'ArrowUp' || k === 'ArrowDown')) { const names = tree().filter((n) => n.b).map((n) => n.b.name); let i = names.indexOf(S.selected); if (i >= 0) { i = (i + (k === 'ArrowDown' ? 1 : names.length - 1)) % names.length; await openOnly(names[i]); } e.preventDefault(); return; }
  if (!inputIds.has(e.target.id) && k.length === 1 && !ctrl && !e.altKey) $('input').focus();
});
function failed(err) {
  const text = String(err?.message ?? err);
  if (S.setup?.open) { S.setup.error = text; renderSetup(); } else toast(text, 4000);
}
$('setup').addEventListener('change', (e) => { if (e.target.id === 'setupmodel') followModel(e.target.value, 'setupeffort', true); });
$('setup').addEventListener('submit', async (e) => {
  e.preventDefault();
  const form = e.target;
  try {
    if (form.id === 'setupform') await connectProvider(S.setup.adding, Object.fromEntries([...form.querySelectorAll('input, select')].map((el) => [el.name, el.value.trim()])));
    else if (form.id === 'setupproj') { const dir = $('setupdir').value.trim(), model = $('setupmodel').value; if (dir) { await createProject(dir, model, $('setupeffort')?.value ?? null); closeSetup(); } }
  } catch (err) { failed(err); }
});
async function act(el) {
  const a = el.dataset.act, who = el.dataset.who, v = el.dataset.v, pane = el.dataset.pane, rect = el.getBoundingClientRect?.();
  switch (a) {
    case 'more': showMenu(botMenuItems(who), { rect }, who); return;
    case 'model': await modelMenu(pane, { rect, up: true }); return;
    case 'sendmenu': showMenu(sendMenuItems(pane), { rect, up: true }); return;
    case 'set-model': setModel(who, v); render(); focusInput(who === S.ui.side ? 'side' : 'main'); return;
    case 'set-effort': setEffort(who, v); render(); focusInput(who === S.ui.side ? 'side' : 'main'); return;
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
    case 'close-file': closeFile(); return;
    case 'new-project': showNewProject(true); return;
    case 'new-swarm': await openSwarmSheet(leadProject(who)); return;
    case 'close-sheet': closeSheet(); return;
    case 'mix-add': mixAdd(); return;
    case 'mix-remove': mixRemove(Number(v)); return;
    case 'swarm-stop': await stopSwarm(swarmOf(who)); return;
    case 'swarm-add': await addAgent(swarmOf(who)); return;
    case 'swarm-tab': { const sw = swarmOf(S.selected); if (sw) { sw.tab = v; await enqueue(loadVisible); render(); } return; }
    case 'swarm-filter': { const sw = swarmOf(S.selected); if (sw) { sw.filter = v || null; sw.tab = 'board'; render(); } return; }
    case 'swarm-decide': { const sw = swarmOf(S.selected), [id, yes] = v.split(':'); if (sw) await decide(sw, id, yes === 'yes'); return; }
    case 'settings': await openSetup(); return;
    case 'replace-daemon': {
      if (el.disabled) return;
      // No reattach starts a daemon while the old one is still closing its store.
      el.disabled = true; el.textContent = 'Restarting…'; S.replacing = true; clearTimeout(retryTimer);
      try { await Daemon.replaceDaemon(); S.replacing = false; attach(); }
      catch (e) { S.replacing = false; el.disabled = false; el.textContent = 'Restart the daemon'; toast(String(e?.message ?? e), 6000); retryAttach(); }
      return;
    }
    case 'setup-close': closeSetup(); return;
    case 'setup-add': setupState().adding = ''; renderSetup(); return;
    case 'setup-pick': setupState().adding = v; renderSetup(); $('setup').querySelector('#setupform input')?.focus(); return;
    case 'setup-cancel': setupState().adding = null; renderSetup(); return;
    // Removing restarts the daemon; with agents working, the first press says so and the second removes.
    case 'setup-remove': { const st = setupState(); if (anyActive() && st.confirm !== v) { st.confirm = v; renderSetup(); return; } st.confirm = null; await removeProvider(v); return; }
    case 'setup-retry': case 'setup-refresh': await refreshModels(); return;
    case 'edit-role': await editRole(v); return;
    case 'schedules-first': await readSchedules(); renderSetup(); return;
    case 'schedules-next': await readSchedules(setupState().schedulesNext); renderSetup(); return;
    case 'schedule-remove': await removeSchedule(v); return;
    case 'open-host': await Daemon.openHost(v); return;
    default: return;
  }
}
document.addEventListener('click', async (e) => {
  if (S.ui.help) { hideHelp(); return; }
  if (e.target.closest?.('#sheetwrap') && !e.target.closest('#sheet')) { closeSheet(); return; }
  if (e.target.closest('#pickerwrap') && !e.target.closest('.picker')) { closePicker(); return; }
  if (Rich.click(e)) { closeMenu(); return; }
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

// Highlighting arrived: messages drawn without it are drawn again.
Rich.onReady = () => { for (const [id] of PANES) $(id).dataset.key = ''; render(); };
Rich.onFile = openFileFrom;
Rich.onError = (text) => toast(text, 4000);

// ---------- boot ----------
render();
attach().then(() => $('input').focus());
})();
