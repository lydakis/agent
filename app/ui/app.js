// The Thread client. State is built from the daemon's event stream exactly
// as the terminal client builds it; rendering is the prototype's, verbatim.
(() => {
'use strict';
const $ = (id) => document.getElementById(id);
const GLYPH = { running: '●', waiting: '◐', paced: '◔', failed: '✘', idle: '○', queued: '◌', interrupted: '✘' };
const LABEL = { running: 'working', waiting: 'waiting', paced: 'rate limited', failed: 'failed', idle: 'idle', queued: 'queued', interrupted: 'interrupted' };
const LAZY_ITEMS = 400;

const S = {
  bots: new Map(), transcripts: new Map(), selected: '', cursor: 0, live: false, attached: false, autoSelect: true,
  config: null, ui: { rail: false, peek: null, picker: false, pickerSel: 0, help: false, thoughts: false, output: false, toast: null },
};
const sessionKey = () => `agent:${S.config?.socket}|${S.config?.workspace}`;
const bot = (name) => S.bots.get(name);
const transcript = (name) => { if (!S.transcripts.has(name)) S.transcripts.set(name, { items: [], nodes: 0, text: '', thinking: '', thinkingSince: 0, streamingTurn: null }); return S.transcripts.get(name); };
// Every bare node goes through here so the count stays right; loads skip a transcript at zero.
const pushNode = (t, item) => { t.nodes += 1; t.items.push(item); };
const glyphOf = (status) => GLYPH[status] || '✘';
const labelOf = (status) => LABEL[status] || 'failed';
const fmt = (ms) => { const s = Math.max(0, Math.round(ms / 1000)); return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m${String(s % 60).padStart(2, '0')}s`; };
const esc = (s) => String(s).replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]));
function toast(text, ms = 2200) { S.ui.toast = text; render(); setTimeout(() => { if (S.ui.toast === text) { S.ui.toast = null; render(); } }, ms); }

// ---------- bots ----------
function upsert(record) {
  if (!record?.name) return;
  // Same name, different identity: everything known about the old bot belongs to the old bot.
  const known = bot(record.name);
  if (known && known.id != null && record.id != null && known.id !== record.id) { S.bots.delete(record.name); S.transcripts.delete(record.name); }
  const b = bot(record.name) || { name: record.name, id: null, parent: null, waitingOn: [], turnStarted: 0, elapsed: 0 };
  if (record.id != null) b.id = record.id;
  b.status = record.status === 'completed' ? 'idle' : (record.status || 'idle');
  b.runningTurn = record.running_turn ?? null;
  b.model = `${record.provider ?? '?'}/${record.model ?? '?'}`;
  b.workspace = record.workspace ?? null;
  if (record.created_by) b.parent = record.created_by;
  S.bots.set(b.name, b);
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
  for (const b of S.bots.values()) { const key = b.parent && S.bots.has(b.parent) ? b.parent : null; if (!children.has(key)) children.set(key, []); children.get(key).push(b); }
  const out = []; const seen = new Set(); const stack = [];
  const pushKids = (parent, depth, cont) => {
    const kids = (children.get(parent) ?? []).filter((b) => !seen.has(b.name));
    for (let i = kids.length - 1; i >= 0; i--) stack.push([kids[i], depth, i === kids.length - 1, cont]);
  };
  pushKids(null, 0, '');
  for (;;) {
    while (stack.length) {
      const [b, depth, last, cont] = stack.pop();
      if (seen.has(b.name)) continue; seen.add(b.name);
      const prefix = depth === 0 ? '' : cont + (last ? '└ ' : '├ ');
      out.push({ b, depth, prefix });
      // The continuation stops growing past a few levels: a chain of thousands must not cost thousands per row.
      pushKids(b.name, depth + 1, depth === 0 ? '' : depth > 6 ? cont : cont + (last ? '  ' : '│ '));
    }
    // A creator cycle (delete and recreate) reaches nothing from the roots; root it so nothing is hidden.
    const orphan = [...S.bots.values()].find((b) => !seen.has(b.name));
    if (!orphan) break;
    stack.push([orphan, 0, true, '']);
  }
  return out;
}
function callSummary(name, args) {
  let a = {}; try { a = JSON.parse(args); } catch (_) {}
  let s = name === 'shell' ? a.command ?? '' : ['read', 'write', 'edit'].includes(name) ? a.path ?? '' : name === 'wait' ? (a.handles || []).map((h) => h.replace(/^turn:/, '')).join(', ') : args;
  return String(s).split('\n')[0].slice(0, 300);
}

// ---------- events ----------
async function onEvent(ev) {
  const kind = ev.event, name = ev.bot ?? '', turn = ev.turn ?? null, data = ev.data ?? {};
  if (typeof ev.cursor === 'number') S.cursor = Math.max(S.cursor, ev.cursor);
  switch (kind) {
    case 'follow_live': {
      S.live = true;
      if (S.autoSelect) { const first = tree()[0]; if (first) S.selected = first.b.name; }
      S.autoSelect = false;
      break;
    }
    case 'follow_lagged': S.attached = false; toast('event stream lagged; attaching again'); await attach(); return true;
    case 'closed': S.attached = false; S.live = false; showDetached('the daemon closed the session'); return true;
    case 'text_delta': { const t = transcript(name); t.streamingTurn = turn; t.text += ev.text ?? ''; break; }
    case 'thinking_delta': { const t = transcript(name); t.streamingTurn = turn; if (!t.thinkingSince) t.thinkingSince = Date.now(); t.thinking += ev.text ?? ''; break; }
    case 'created': case 'forked': {
      // The snapshot already holds every bot that existed at attach; only a bot born after it needs a fetch.
      if (!S.bots.has(name)) await refreshBot(name);
      if (data.created_by && bot(name)) bot(name).parent = data.created_by;
      if (kind === 'created') {
        // Lineage comes from the daemon's record or the event, never from guessing at shell text.
        const parent = bot(name)?.parent;
        if (parent && bot(name) && S.bots.has(parent)) { bot(name).parent = parent; transcript(parent).items.push({ kind: 'peer', who: name, turn: bot(parent)?.runningTurn ?? null }); }
      } else transcript(name).items.push({ kind: 'note', text: `forked from ${data.source ?? '?'}`, turn: null });
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
      transcript(name).items.push({ kind: 'note', text: behindOwn ? 'queued behind the running turn' : 'queued for a slot', turn });
      break;
    }
    case 'message': {
      const t = transcript(name);
      if (t.streamingTurn === turn) {
        if (t.thinking) { t.items.push({ kind: 'thought', text: t.thinking, secs: t.thinkingSince ? Math.round((Date.now() - t.thinkingSince) / 1000) : 0, turn }); t.thinking = ''; t.thinkingSince = 0; }
        t.text = '';
      }
      if (typeof data.node === 'number') pushNode(t, { kind: 'node', node: data.node, turn });
      break;
    }
    case 'tool_started': {
      const args = data.arguments ?? '';
      let parsed = {}; try { parsed = JSON.parse(args); } catch (_) {}
      const tname = data.name ?? 'tool';
      transcript(name).items.push({ kind: 'tool', callId: data.call_id, name: tname, summary: callSummary(tname, args), args, background: tname === 'shell' && parsed.background === true, spawns: tname === 'shell' && spawnsPeer(String(parsed.command ?? '')), done: false, started: S.live ? Date.now() : 0, took: 0, turn });
      break;
    }
    case 'tool_completed': {
      const t = transcript(name);
      const call = [...t.items].reverse().find((i) => i.kind === 'tool' && i.callId === data.call_id);
      if (call) { call.done = true; if (call.started) call.took = Date.now() - call.started; call.started = 0; }
      if (typeof data.node === 'number') {
        pushNode(t, { kind: 'node', node: data.node, callId: data.call_id, turn });
        if (call && (call.background || call.name === 'wait')) {
          await loadWaitOrProc(name, data.node, call);
          // The node is spent: the cards show its result, and a later lazy load must not decode it again.
          const pos = t.items.findLastIndex((it) => it.kind === 'node' && it.node === data.node);
          if (pos >= 0) { t.items.splice(pos, 1); t.nodes = Math.max(0, t.nodes - 1); t.gen = (t.gen ?? 0) + 1; }
        }
      }
      break;
    }
    case 'turn_waiting': { const b = bot(name); if (b) { b.status = 'waiting'; b.waitingOn = data.handles ?? []; } break; }
    case 'turn_paced': { const b = bot(name); if (b) b.status = 'paced'; break; }
    case 'turn_resumed': { const b = bot(name); if (b) { b.status = 'running'; b.waitingOn = []; } break; }
    case 'steered': transcript(name).items.push({ kind: 'note', text: 'steered into the running turn', turn }); break;
    case 'turn_finished': {
      const status = data.status ?? '?';
      const b = bot(name);
      // A steer absorbed into a running turn finishes as its own turn while that turn goes on.
      if (b && (b.runningTurn === null || b.runningTurn === turn)) { b.runningTurn = null; b.waitingOn = []; if (b.turnStarted) b.elapsed = Date.now() - b.turnStarted; b.turnStarted = 0; b.status = status === 'completed' || status === 'steered' ? 'idle' : status; }
      const t = transcript(name);
      if (t.streamingTurn === turn) { if (t.text) t.items.push({ kind: 'text', text: t.text, turn }); t.text = ''; t.thinking = ''; t.thinkingSince = 0; t.streamingTurn = null; }
      if (status !== 'completed' && status !== 'steered') t.items.push({ kind: 'note', text: data.error ? `${status}: ${data.error}${data.detail ? ': ' + data.detail : ''}` : status, turn });
      // A background command may outlive its turn; only a wait result says how it ended.
      break;
    }
    case 'deleted': S.bots.delete(name); S.transcripts.delete(name); if (S.selected === name) S.selected = S.bots.keys().next().value ?? ''; if (S.ui.peek === name) S.ui.peek = null; break;
    case 'pruned': {
      // A `follow *` replay reports a retention gap with bot "*": a notice about the store, not a transcript.
      if (name === '*') toast(`events before cursor ${ev.before ?? 0} were pruned; older history is gone`, 5000);
      else transcript(name).items.push({ kind: 'note', text: 'earlier history pruned', turn: null });
      break;
    }
    default: break;
  }
}
async function loadWaitOrProc(name, node, call) {
  let item; try { item = await Daemon.request('item', { bot: name, node }); } catch (_) { return; }
  applyWaitOrProc(name, item, call);
}
// Decode a background start (a proc handle) or a wait result into the cards; also reached by a retried load.
function applyWaitOrProc(name, item, call) {
  const output = item.output ?? item.content?.[0]?.content ?? '';
  let value; try { value = JSON.parse(output); } catch (_) { return; }
  const t = transcript(name);
  if (call.background) {
    if (typeof value.handle === 'string') t.items.push({ kind: 'proc', handle: value.handle, cmd: call.summary, done: null, open: false, turn: bot(name)?.runningTurn ?? null });
  } else if (value.results) {
    for (const [handle, result] of Object.entries(value.results)) {
      if (result.pending) continue;
      for (const it of t.items) if (it.kind === 'proc' && it.handle === handle) {
        const out = result.stdout ?? result.output ?? '';
        // A process can end without an exit status: a spawn failure, a timeout, an output limit. Say which.
        if (result.error) it.done = result.detail ? `${result.error}: ${result.detail}` : String(result.error);
        else if (typeof result.exit_code === 'number' && result.exit_code !== 0) it.done = `exit ${result.exit_code}`;
        else if (result.success === false) it.done = 'failed';
        else it.done = out.trimEnd().split('\n').pop() ?? '';
      }
    }
  }
}
function entries(item) {
  const out = [];
  const text = (content, keys) => Array.isArray(content) ? content.filter((p) => keys.includes(p.type)).map((p) => p.text ?? '').join('') : typeof content === 'string' ? content : '';
  const shell = (o) => { let v; try { v = JSON.parse(o); } catch (_) { return o; } if (!v || typeof v !== 'object' || !('stdout' in v)) return o; let s = (v.stdout ?? '').trimEnd(); if (v.stderr?.trim()) s += (s ? '\n' : '') + 'stderr: ' + v.stderr.trimEnd(); if (v.exit_code) s += (s ? '\n' : '') + `exit ${v.exit_code}`; return s || '(no output)'; };
  if (item.type === 'function_call_output') return [{ kind: 'out', text: shell(item.output ?? '') }];
  if (item.type === 'function_call') return out;
  if (item.type === 'reasoning') { const s = text(item.summary, ['summary_text']); if (s) out.push({ kind: 'thought', text: s, secs: 0 }); return out; }
  if (item.role === 'user') {
    if (Array.isArray(item.content)) { let t = ''; for (const p of item.content) { if (p.type === 'tool_result') out.push({ kind: 'out', text: shell(text(p.content, ['text'])) }); else if (p.type === 'text' || p.type === 'input_text') t += p.text ?? ''; } if (t) out.unshift({ kind: 'user', text: t }); }
    else out.push({ kind: 'user', text: text(item.content, ['text']) });
  } else if (item.role === 'assistant' && Array.isArray(item.content)) {
    let t = ''; for (const p of item.content) { if (p.type === 'thinking' && p.thinking) out.push({ kind: 'thought', text: p.thinking, secs: 0 }); else if (p.type === 'text' || p.type === 'output_text') t += p.text ?? ''; } if (t) out.push({ kind: 'text', text: t });
  }
  return out;
}
async function load(name) {
  // One batch of the newest bare nodes: what the pane can show. Scrolling up asks for the next
  // batch, so a long history is materialized only as far as someone reads.
  await loadBatch(name);
}
async function loadBatch(name) {
  const t = S.transcripts.get(name); if (!t || !t.nodes) return false;
  const pending = t.items.map((it, i) => [i, it]).filter(([, it]) => it.kind === 'node').slice(-LAZY_ITEMS).reverse();
  if (!pending.length) { t.nodes = 0; return false; }
  const fetched = await Promise.all(pending.map(([, it]) => Daemon.request('item', { bot: name, node: it.node }).then((v) => ({ ok: v }), (e) => ({ err: String(e?.message ?? e) }))));
  let progressed = false; const deferred = [];
  pending.forEach(([index, it], k) => {
    const r = fetched[k];
    // A lost session is not the item's fault: the node stays and the next attach fetches it. Anything else is final.
    if (r.err && /daemon_disconnected|detached|^io\b/.test(r.err)) return;
    progressed = true; t.nodes = Math.max(0, t.nodes - 1);
    let es = r.ok ? entries(r.ok) : [{ kind: 'note', text: `node ${it.node}: ${r.err}` }];
    const call = it.callId ? t.items.slice(0, index).reverse().find((x) => x.kind === 'tool' && x.callId === it.callId) : null;
    if (call && r.ok && (call.background || call.name === 'wait')) deferred.push([call, r.ok]);
    if (call && (call.background || call.spawns || call.name === 'wait')) es = es.filter((e) => e.kind !== 'out');
    const rep = [];
    for (const e of es) {
      if (e.kind === 'thought') { const prev = t.items[index - 1]; if (prev && prev.kind === 'thought' && prev.turn === it.turn && rep.length === 0) { prev.text = e.text; continue; } }
      rep.push({ ...e, turn: it.turn });
    }
    t.items.splice(index, 1, ...rep);
    t.gen = (t.gen ?? 0) + 1;
  });
  for (const [call, item] of deferred) applyWaitOrProc(name, item, call);
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
let unlisten = null;
async function attach() {
  try {
    if (!S.config) S.config = await Daemon.setup();
    if (!unlisten) unlisten = await Daemon.onEvent((ev) => enqueue(async () => {
      // An event from a session this page already left behind is noise.
      if (ev.session !== undefined && S.session !== undefined && ev.session !== S.session) return;
      const terminal = await onEvent(ev);
      // During replay nothing is fetched: a load per node-producing event would serialize a long history
      // into one request each. The first load runs once follow_live arrives.
      if (!terminal) { if (S.live) await loadVisible(); render(); }
    }));
    const result = await Daemon.attach(S.cursor);
    if (result.session !== undefined) S.session = result.session;
    // The snapshot is authoritative: a bot deleted while this page had no session is gone from it and
    // its live-only `deleted` notice cannot be replayed; anything newer arrives on the subscription.
    const listed = new Set((result.bots ?? []).map((r) => r.name));
    for (const record of result.bots ?? []) upsert(record);
    for (const name of [...S.bots.keys()]) if (!listed.has(name)) { S.bots.delete(name); S.transcripts.delete(name); if (S.ui.peek === name) S.ui.peek = null; }
    S.attached = true;
    restore();
    await enqueue(loadVisible);
    $('detached').classList.remove('on');
    render();
    return true;
  } catch (e) {
    Daemon.log?.(`attach failed: ${e?.message ?? e}`);
    showDetached(String(e?.message ?? e));
    return false;
  }
}
function showDetached(reason) {
  S.attached = false;
  $('detached').innerHTML = `<div><b>not attached</b></div><div>${esc(reason)}</div><div style="margin-top:8px">daemon at <span class="k">${esc(S.config?.socket ?? '?')}</span> · retrying</div>`;
  $('detached').classList.add('on');
  setTimeout(() => { if (!S.attached) attach(); }, 2000);
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
function cardHTML({ attr, status, name, last, elapsed, sel, body }) {
  return `<div class="peer${sel ? ' sel' : ''}" ${attr} role="button" tabindex="0"><span class="glyph ${status}">${glyphOf(status)}</span><span class="pn">${esc(name)}</span><span class="el">${elapsed ?? ''}</span><span class="pl">${esc(last)}</span>${body ?? ''}</div>`;
}
function lastLine(t) {
  if (t.text) return t.text; if (t.thinking) return t.thinking.split('. ').pop();
  for (const it of [...t.items].reverse()) { if (it.kind === 'text') return it.text; if (it.kind === 'tool') return `▸ ${it.name} ${it.summary}`; }
  return '';
}
function itemHTML(it) {
  switch (it.kind) {
    case 'user': return `<div class="line user">› ${esc(it.text)}</div>`;
    case 'text': return markdown(it.text);
    case 'thought': return S.ui.thoughts ? `<div class="line think">${esc(it.text)}</div>` : `<div class="line think folded">thought ${fmt((it.secs || 0) * 1000)}</div>`;
    case 'tool': { const el = it.started ? `<span class="el">${fmt(Date.now() - it.started)}</span>` : it.took >= 1500 ? `<span class="el">${fmt(it.took)}</span>` : ''; return `<div class="line tool">▸ <b>${esc(it.name)}</b> ${esc(it.summary)}${el}</div>`; }
    case 'out': { const rows = it.text.split('\n').filter((l) => l.trim()); const shown = !S.ui.output && rows.length > 2 ? rows.slice(0, 2) : rows; return `<div class="line out">${esc(shown.join('\n'))}${shown.length < rows.length ? ` <span class="more">+${rows.length - shown.length} lines</span>` : ''}</div>`; }
    case 'note': return `<div class="line note">${esc(it.text)}</div>`;
    case 'peer': { const p = bot(it.who); if (!p) return ''; const t = transcript(it.who); const el = p.turnStarted ? fmt(Date.now() - p.turnStarted) : p.elapsed ? fmt(p.elapsed) : ''; return cardHTML({ attr: `data-peek="${esc(p.name)}"`, status: p.status, name: p.name, last: lastLine(t), elapsed: el, sel: S.ui.peek === it.who }); }
    case 'proc': { const status = it.done === null ? 'running' : 'idle'; const last = it.done === null ? it.handle : (it.done || 'done'); return cardHTML({ attr: `data-proc="${esc(it.handle)}"`, status, name: `$ ${it.cmd}`, last, elapsed: '', sel: false }); }
    case 'node': return `<div class="line pending">…</div>`;
    default: return '';
  }
}
function itemsHTML(t) {
  let h = ''; let lastTurn = null;
  for (const it of t.items) { if (it.turn != null && it.turn !== lastTurn) { if (h) h += '<div class="line"></div>'; lastTurn = it.turn; } h += itemHTML(it); }
  return h;
}
function tailHTML(name, t) {
  if (t.thinking) return `<div class="line think">${esc(t.thinking.split('. ').pop())}<span class="cursor"></span></div>`;
  if (t.text) return markdown(t.text).replace(/<\/div>$/, '<span class="cursor"></span></div>');
  if (bot(name)?.status === 'running') return `<div class="line text"><span class="cursor"></span></div>`;
  return '';
}
// A streamed delta touches only the tail. The items rebuild when their
// count or a fold changes, when a load replaced nodes (gen), or on the
// slow tick that refreshes elapsed counters and cards.
let forceRebuild = false;
function renderTranscript(el, name) {
  const t = S.transcripts.get(name);
  const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  const before = el.scrollHeight;
  if (!t) { el.innerHTML = ''; return; }
  const key = `${name}|${t.items.length}|${t.gen ?? 0}|${S.ui.thoughts}|${S.ui.output}|${S.ui.peek}`;
  let tail = el.lastElementChild;
  if (forceRebuild || el.dataset.key !== key || !tail || !tail.classList.contains('tail')) {
    el.innerHTML = itemsHTML(t) + '<div class="tail"></div>';
    el.dataset.key = key;
    tail = el.lastElementChild;
    // History loaded above the reader keeps their place instead of shoving it down.
    if (!atBottom) el.scrollTop += el.scrollHeight - before;
  }
  tail.innerHTML = tailHTML(name, t);
  if (atBottom) el.scrollTop = el.scrollHeight;
}
for (const [id, who] of [['log', () => S.selected], ['peek', () => S.ui.peek]]) {
  $(id).addEventListener('scroll', () => {
    const el = $(id); const name = who();
    if (el.scrollTop < 200 && name && (S.transcripts.get(name)?.nodes ?? 0) > 0) enqueue(async () => { await load(name); render(); });
  });
}
function titleHTML(b, closable) { return `<span class="glyph ${b.status}">${glyphOf(b.status)}</span><b>${esc(b.name)}</b><span>${labelOf(b.status)}</span>${closable ? '<span class="x">Esc closes</span>' : ''}`; }
function botRowHTML(n, sel) {
  const b = n.b;
  const w = b.waitingOn.length ? `<div class="w" style="padding-left:${3 + n.depth * 2}ch">⏳ ${b.waitingOn.map((h) => h.replace(/^turn:/, '').split('/')[0]).join(' ')}</div>` : '';
  return `<div class="botrow${sel ? ' sel' : ''}" data-bot="${esc(b.name)}" role="button" tabindex="0"><span class="tree">${n.prefix}</span><span class="glyph ${b.status}">${glyphOf(b.status)}</span><span class="n">${esc(b.name)}</span></div>${w}`;
}
function peers() { return (S.transcripts.get(S.selected)?.items ?? []).filter((i) => i.kind === 'peer' && S.bots.has(i.who)).map((i) => i.who); }
function keybarHTML(b) {
  const busy = b && isActive(b.status);
  const dot = `<span><span class="dot${!S.attached ? ' off' : busy ? ' busy' : ''}"></span>${!S.attached ? 'detached' : busy ? labelOf(b.status) : 'live'}</span>`;
  const keys = [];
  if (S.ui.picker) keys.push('<kbd>↑↓</kbd> choose', '<kbd>Enter</kbd> switch', '<kbd>Esc</kbd> cancel');
  else if (S.ui.peek) keys.push('<kbd>Esc</kbd> close', '<kbd>^p</kbd> next peer', '<kbd>^k</kbd> switch');
  else {
    keys.push('<kbd>^k</kbd> switch'); if (peers().length) keys.push('<kbd>^p</kbd> peek'); keys.push('<kbd>^b</kbd> bots');
    const t = S.transcripts.get(S.selected);
    if (t?.items.some((i) => i.kind === 'thought')) keys.push(`<kbd>^t</kbd> ${S.ui.thoughts ? 'fold' : 'thoughts'}`);
    if (t?.items.some((i) => i.kind === 'out' && i.text.split('\n').length > 2)) keys.push(`<kbd>^o</kbd> ${S.ui.output ? 'fold' : 'output'}`);
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
  if (S.ui.rail) $('bots').innerHTML = tree().map((n) => botRowHTML(n, n.b.name === S.selected)).join('');
  if (S.ui.peek && bot(S.ui.peek)) { $('peektitle').innerHTML = titleHTML(bot(S.ui.peek), true); renderTranscript($('peek'), S.ui.peek); }
  forceRebuild = false;
  $('who').textContent = b ? `${b.name} ›` : '›';
  $('input').placeholder = b ? (b.status === 'idle' ? '' : `${b.name} is ${labelOf(b.status)}; your message queues`) : '/new NAME [PROVIDER/MODEL]';
  $('keybar').innerHTML = keybarHTML(b);
  if (S.ui.picker) renderPicker();
}
setInterval(() => { if (S.attached && [...S.bots.values()].some((b) => isActive(b.status))) { forceRebuild = true; render(); } }, 1000);

// ---------- picker ----------
function pickerRows() {
  const q = $('pickerq').value.trim().toLowerCase();
  return tree().map((n) => ({ ...n, i: q ? n.b.name.toLowerCase().indexOf(q) : -1 })).filter((r) => !q || r.i >= 0);
}
function renderPicker() {
  const q = $('pickerq').value.trim(); const rows = pickerRows();
  S.ui.pickerSel = Math.min(S.ui.pickerSel, Math.max(0, rows.length - 1));
  $('pickerlist').innerHTML = rows.length ? rows.map((r, idx) => {
    const n = r.b.name; const hit = r.i >= 0 ? `${esc(n.slice(0, r.i))}<span class="hit">${esc(n.slice(r.i, r.i + q.length))}</span>${esc(n.slice(r.i + q.length))}` : esc(n);
    const state = r.b.status === 'idle' ? '' : labelOf(r.b.status);
    const hint = q ? [r.b.parent ? `↳ ${r.b.parent}` : '', state].filter(Boolean).join(' · ') : state;
    return `<div class="row${idx === S.ui.pickerSel ? ' sel' : ''}" data-pick="${esc(n)}">${q ? '' : `<span class="tree">${r.prefix}</span>`}<span class="glyph ${r.b.status}">${glyphOf(r.b.status)}</span><span class="n">${hit}</span><span class="h">${esc(hint)}</span></div>`;
  }).join('') : '<div class="empty">no bot matches</div>';
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
  await Daemon.request('submit', { bot: b.name, request_id: `app-${Date.now()}`, prompt: text, workspace: b.workspace ?? S.config.workspace, delivery: b.status === 'idle' ? 'reject' : 'queue' });
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
  const row = e.target.closest('[data-bot]'); const peek = e.target.closest('[data-peek]'); const proc = e.target.closest('[data-proc]');
  if (row) await switchTo(row.dataset.bot);
  else if (peek) { S.ui.peek = S.ui.peek === peek.dataset.peek ? null : peek.dataset.peek; await enqueue(loadVisible); render(); save(); }
  else if (proc) { const t = S.transcripts.get(S.selected); const it = t?.items.find((i) => i.kind === 'proc' && i.handle === proc.dataset.proc); if (it) { it.open = !it.open; render(); } }
  if (!e.target.closest('input')) $('input').focus();
});

// ---------- boot ----------
render();
attach().then(() => $('input').focus());
})();
