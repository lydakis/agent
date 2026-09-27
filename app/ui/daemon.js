// The page's only door to the daemon. Inside Tauri it is the Rust core:
// `setup`, `attach`, `request`, and `daemon` window events. In a plain
// browser it is a small simulated daemon speaking the same protocol shapes,
// so the design can be worked on without a daemon at all.
window.Daemon = (() => {
  const tauri = window.__TAURI__;
  if (tauri) {
    const { invoke } = tauri.core;
    const log = (m) => invoke('log', { message: String(m) }).catch(() => {});
    window.addEventListener('error', (e) => log(`error: ${e.message} @${e.filename}:${e.lineno}`));
    window.addEventListener('unhandledrejection', (e) => log(`rejection: ${e.reason?.message ?? e.reason}`));
    return {
      log,
      setup: () => invoke('setup'),
      defaultModel: () => invoke('default_model'),
      policy: (workspace) => invoke('policy', { workspace: workspace ?? null }),
      models: () => invoke('models'),
      project: (dir) => invoke('project', { dir }),
      writeProject: ({ dir, name, model }) => invoke('write_project', { dir, name, model }),
      branch: (dir) => invoke('branch', { dir }),
      attach: (after) => invoke('attach', { after }),
      pull: (session) => invoke('pull', { session }),
      request: (op, params = {}) => invoke('request', { op, params }),
      close: () => tauri.window.getCurrentWindow().close(),
    };
  }

  // ---------- demo daemon ----------
  const S = { bots: new Map(), nodes: new Map(), lineages: new Map(), nextNode: 1, nextTurn: 1, nextProc: 1, nextId: 1, cursor: 0, session: 0, queue: [], waiter: null, timers: new Set(), sides: new Set() };
  // Notifications wait in a queue for the page's next pull, as the core's transport holds them.
  const emit = (event) => { if (event.data?.node != null) { if (!S.lineages.has(event.bot)) S.lineages.set(event.bot, []); S.lineages.get(event.bot).push({node:event.data.node,turn:event.turn ?? null}); } if (event.durable !== false) event.cursor = ++S.cursor; S.queue.push(event); if (S.waiter) { const w = S.waiter; S.waiter = null; w(); } };
  const node = (item) => { const id = S.nextNode++; S.nodes.set(id, item); return id; };
  const wait = (ms) => new Promise((r) => { const t = setTimeout(() => { S.timers.delete(t); r(); }, ms); S.timers.add(t); });
  const record = (name, model) => ({ name, status: 'idle', running_turn: null, provider: model.split('/')[0], model: model.split('/').slice(1).join('/'), workspace: '/workspace', input_tokens: 0, cached_input_tokens: 0 });

  async function create(name, model, createdBy = null, source = null, workspace = null, allowed = null) {
    if (S.bots.has(name)) throw new Error('bot_exists');
    // Lineage is pinned to the creator's identity, and the event carries the record's list fields, as the daemon's does.
    const b = { ...record(name, model), ...(workspace ? { workspace } : {}), id: S.nextId++, created_by: createdBy, created_by_id: createdBy ? S.bots.get(createdBy)?.id ?? null : null, turns: 0, interrupted: false, ...(allowed ? { allowed } : {}) };
    S.bots.set(name, b);
    // A fork shares its source's history up to its newest finished round. The demo keeps no call
    // nodes, only their results, so that is its newest node that is not a tool result.
    if (source) {
      const all = S.lineages.get(source) ?? [];
      let end = all.length; while (end > 0 && S.nodes.get(all[end - 1].node)?.type === 'function_call_output') end--;
      S.lineages.set(name, all.slice(0, end));
    }
    const checkpoint = source ? S.lineages.get(source)?.at(-1)?.node ?? null : undefined;
    emit({ event: source ? 'forked' : 'created', bot: name, turn: null, data: { id: b.id, provider: b.provider, model: b.model, workspace: b.workspace, status: 'idle', running_turn: null, created_by: createdBy, created_by_id: b.created_by_id, ...(allowed ? { allowed } : {}), ...(source ? { source, checkpoint } : {}) } });
    return b;
  }
  // Scripted work outlives a stop; a bot deleted meanwhile reads as interrupted, so it ends quietly.
  const GONE = { interrupted: true, status: 'idle' };
  async function stream(name, turn, text, pace = 40) {
    const b = S.bots.get(name) ?? GONE;
    for (const word of text.split(' ')) {
      if (b.interrupted) return false;
      emit({ event: 'text_delta', bot: name, turn, text: word + ' ', durable: false });
      await wait(pace + Math.random() * pace);
    }
    emit({ event: 'message', bot: name, turn, data: { node: node({ type: 'message', role: 'assistant', content: [{ type: 'output_text', text }] }) } });
    await steerIn(name, turn);
    return true;
  }
  // A steer waits for the running turn's next round boundary, joins it as a user message, and the
  // scripted model acknowledges it before carrying on.
  async function steerIn(name, turn) {
    const b = S.bots.get(name) ?? GONE;
    while (b.steers?.length && !b.interrupted) {
      const prompt = b.steers.shift();
      emit({ event: 'message', bot: name, turn, data: { node: node({ role: 'user', content: [{ type: 'input_text', text: prompt }] }) } });
      await wait(200);
      await stream(name, turn, `Noted: ${prompt.trim().replace(/[.?!]+$/, '')}. Carrying on with that in mind.`);
    }
  }
  async function think(name, turn, text) {
    const b = S.bots.get(name) ?? GONE;
    for (const word of text.split(' ')) {
      if (b.interrupted) return false;
      emit({ event: 'thinking_delta', bot: name, turn, text: word + ' ', durable: false });
      await wait(35);
    }
    emit({ event: 'message', bot: name, turn, data: { node: node({ type: 'reasoning', summary: [{ type: 'summary_text', text }] }) } });
    return true;
  }
  let calls = 0;
  async function tool(name, turn, tname, args, output, ms = 500) {
    const b = S.bots.get(name) ?? GONE;
    // A tool outside the bot's allowed list is never called.
    if (b.interrupted || (b.allowed && !b.allowed.includes(tname))) return;
    const call_id = `call_${++calls}`;
    emit({ event: 'tool_started', bot: name, turn, data: { call_id, name: tname, arguments: JSON.stringify(args), arguments_truncated: false } });
    await wait(ms);
    if (b.interrupted) return;
    emit({ event: 'tool_completed', bot: name, turn, data: { call_id, node: node({ type: 'function_call_output', call_id, output }), artifacts: [] } });
    await steerIn(name, turn);
  }
  function start(name, prompt) {
    const b = S.bots.get(name);
    if (b.status !== 'idle') { emit({ event: 'queued', bot: name, turn: S.nextTurn, data: { delivery: 'queue' } }); return null; }
    const turn = S.nextTurn++;
    b.turns++; b.running_turn = turn; b.status = 'running'; b.interrupted = false; b.steers = [];
    emit({ event: 'accepted', bot: name, turn, data: { request_id: `demo-${turn}`, node: node({ role: 'user', content: [{ type: 'input_text', text: prompt }] }), workspace: b.workspace, model: `${b.provider}/${b.model}` } });
    return turn;
  }
  function finish(name, turn, status = 'completed') {
    const b = S.bots.get(name); if (!b) return;
    b.status = 'idle'; b.running_turn = null;
    emit({ event: 'turn_finished', bot: name, turn, data: { status, checkpoint: status === 'completed' ? S.nextNode - 1 : null, error: status === 'completed' ? null : 'cancelled', detail: null } });
  }
  async function reply(name, prompt) {
    const turn = start(name, prompt);
    if (turn === null) return;
    await wait(250);
    if (/scenario|ship|split/i.test(prompt)) { await scenario(name, turn); return; }
    // A fork given its own tool list stands in for a side chat, which
    // answers from the history it was forked with.
    if (S.sides.has(name)) {
      await tool(name, turn, 'read', { path: 'PLAN.md' }, '1.2 KiB · three steps', 400);
      await stream(name, turn, 'Waiting on three peers: plan is done, build is waiting on its reviewer, and test is running. The release build is still going in the background.');
      if (!(S.bots.get(name) ?? GONE).interrupted) finish(name, turn);
      return;
    }
    if (/test|check|run/i.test(prompt)) {
      await tool(name, turn, 'shell', { command: 'cargo test -p agent-runtime' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: 'running 80 tests\ntest result: ok. 80 passed; 0 failed\n', success: true }), 900);
      await stream(name, turn, 'All green. Eighty tests pass, nothing flaky in the store or delivery suites.');
    } else if (/read|look|what|how/i.test(prompt)) {
      await tool(name, turn, 'read', { path: 'src/server/mod.rs' }, '1540 lines · dispatch at line 837', 400);
      await stream(name, turn, 'The dispatch matches each op to a store call. `wait` is the odd one out: it defers its response to the handle registry, so the session never blocks.');
    } else {
      await stream(name, turn, `Got it. ${prompt.trim().replace(/[.?!]+$/, '')}: I will keep it small and report back with a diff, not a story.`);
    }
    if (!(S.bots.get(name) ?? GONE).interrupted) finish(name, turn);
  }
  async function scenario(name, turn) {
    const m = S.bots.get(name);
    await think(name, turn, 'Three independent pieces here: plan, build, test. Peers are cheap, so spawn all three. The release build is slow and nobody depends on it until the end, so start it in the background now and collect it with the rest.');
    await stream(name, turn, 'Splitting this into three peers, kicking off the release build in the background, and waiting on all of it.');
    const proc = S.nextProc++;
    await tool(name, turn, 'shell', { command: 'cargo build --release', background: true }, JSON.stringify({ handle: `proc:${proc}`, background: true }), 300);
    const tasks = { 'demo.plan': 'Write the change plan for the login fix: files, risks, tests.', 'demo.build': 'Apply the login fix under src/auth and keep the diff tight.', 'demo.test': 'Run the auth suite and the daemon smoke, report failures verbatim.' };
    const replies = {
      'demo.plan': 'Three files touch the session cookie. Risk is the refresh path; it needs a regression test. Plan written to PLAN.md.',
      'demo.build': 'Patched refresh_session to reissue the cookie on rotation. Two files changed, 41 lines, cargo check clean. review signed off with one nit, now a comment in the code.',
      'demo.test': 'Auth suite passes. Daemon smoke passes. One warning about an unused import in tests/auth.rs, harmless.',
    };
    const handles = [];
    for (const n of Object.keys(tasks)) {
      if (m.interrupted) return;
      // The task that edits code gets its own worktree, as the app tells its coordinators; the others read the project folder.
      const tree = n === 'demo.build';
      const at = `"$HOME/.agent/worktrees/${n}"`;
      const cmd = tree ? `git worktree add -b agent/${n} ${at} HEAD && "$AGENT_BIN" run --new --agents --bot ${n} --workspace ${at} --model "$AGENT_MODEL" --detach '${tasks[n]}'`
        : `"$AGENT_BIN" run --new --bot ${n} --model "$AGENT_MODEL" --detach '${tasks[n]}'`;
      const call_id = `call_${++calls}`;
      emit({ event: 'tool_started', bot: name, turn, data: { call_id, name: 'shell', arguments: JSON.stringify({ command: cmd }), arguments_truncated: false } });
      await wait(250);
      await create(n, `${m.provider}/${m.model}`, name, null, tree ? `~/.agent/worktrees/${n}` : m.workspace);
      const t = start(n, tasks[n]);
      handles.push(`turn:${n}/${t}`);
      emit({ event: 'tool_completed', bot: name, turn, data: { call_id, node: node({ type: 'function_call_output', call_id, output: JSON.stringify({ exit_code: 0, stderr: '', stdout: JSON.stringify({ bot: n, handle: `turn:${n}/${t}`, status: 'running', turn: t }) + '\n', success: true }) }), artifacts: [] } });
      work(n, t, replies[n]);
    }
    handles.push(`proc:${proc}`);
    const wid = `call_${++calls}`;
    emit({ event: 'tool_started', bot: name, turn, data: { call_id: wid, name: 'wait', arguments: JSON.stringify({ handles }), arguments_truncated: false } });
    m.status = 'waiting';
    emit({ event: 'turn_waiting', bot: name, turn, data: { call_id: wid, handles, deadline_ms: null, any: false } });
    await wait(9000);
    while (Object.keys(tasks).some((n) => (S.bots.get(n) ?? GONE).status !== 'idle') && !m.interrupted) await wait(200);
    if (m.interrupted) return;
    m.status = 'running';
    emit({ event: 'turn_resumed', bot: name, turn, data: { call_id: wid } });
    const results = {}; for (const h of handles) results[h] = h.startsWith('proc:') ? { exit_code: 0, stdout: 'Finished release profile in 41.2s\n', stderr: '', success: true } : { status: 'completed', text: replies[h.split(':')[1].split('/')[0]] };
    emit({ event: 'tool_completed', bot: name, turn, data: { call_id: wid, node: node({ type: 'function_call_output', call_id: wid, output: JSON.stringify({ pending: [], results }) }), artifacts: [] } });
    await steerIn(name, turn);
    await wait(400);
    await stream(name, turn, 'All of it landed. Plan matches the diff, tests are green with one harmless warning, release build finished. Ready for review: two files, 41 lines.');
    finish(name, turn);
  }
  async function work(n, turn, text) {
    await wait(300);
    if (n === 'demo.plan') { await tool(n, turn, 'read', { path: 'src/auth/' }, 'session.rs refresh.rs cookie.rs · 612 lines', 500); await tool(n, turn, 'write', { path: 'PLAN.md' }, '1.2 KiB', 600); }
    if (n === 'demo.build') {
      await tool(n, turn, 'edit', { path: 'src/auth/session.rs' }, '+23 −8', 900);
      await tool(n, turn, 'shell', { command: 'cargo check -p auth' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: 'Finished dev profile in 2.1s\n', success: true }), 1100);
      // build asks a peer of its own to review, and waits on it: depth two.
      if ((S.bots.get(n) ?? GONE).interrupted) return;
      const cmd = `"$AGENT_BIN" run --new --bot demo.review --model "$AGENT_MODEL" --detach 'Review the auth diff for regressions.'`;
      const call_id = `call_${++calls}`;
      emit({ event: 'tool_started', bot: n, turn, data: { call_id, name: 'shell', arguments: JSON.stringify({ command: cmd }), arguments_truncated: false } });
      await wait(250);
      const b = S.bots.get(n);
      // Created from build's shell, the reviewer works in build's worktree.
      await create('demo.review', `${b.provider}/${b.model}`, n, null, b.workspace);
      const rt = start('demo.review', 'Review the auth diff for regressions.');
      emit({ event: 'tool_completed', bot: n, turn, data: { call_id, node: node({ type: 'function_call_output', call_id, output: JSON.stringify({ exit_code: 0, stderr: '', stdout: JSON.stringify({ bot: 'demo.review', handle: `turn:demo.review/${rt}`, status: 'running', turn: rt }) + '\n', success: true }) }), artifacts: [] } });
      const wid = `call_${++calls}`;
      emit({ event: 'tool_started', bot: n, turn, data: { call_id: wid, name: 'wait', arguments: JSON.stringify({ handles: [`turn:demo.review/${rt}`] }), arguments_truncated: false } });
      b.status = 'waiting';
      emit({ event: 'turn_waiting', bot: n, turn, data: { call_id: wid, handles: [`turn:demo.review/${rt}`], deadline_ms: null, any: false } });
      await tool('demo.review', rt, 'shell', { command: 'git diff --stat' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: 'src/auth/session.rs | 31 +-\nsrc/auth/refresh.rs | 10 +\n', success: true }), 500);
      await stream('demo.review', rt, 'Diff is sound. One nit: the rotation path drops the old cookie before the new one is written; harmless today, worth a comment.', 50);
      finish('demo.review', rt);
      b.status = 'running';
      emit({ event: 'turn_resumed', bot: n, turn, data: { call_id: wid } });
      emit({ event: 'tool_completed', bot: n, turn, data: { call_id: wid, node: node({ type: 'function_call_output', call_id: wid, output: JSON.stringify({ pending: [], results: { [`turn:demo.review/${rt}`]: { status: 'completed', text: 'Diff is sound.' } } }) }), artifacts: [] } });
      await steerIn(n, turn);
    }
    if (n === 'demo.test') { await tool(n, turn, 'shell', { command: 'cargo test -p auth' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: 'test result: ok. 34 passed; 0 failed\n', success: true }), 1600); }
    await stream(n, turn, text, 50);
    finish(n, turn);
  }

  const api = {
    setup: async () => ({ socket: 'demo', model: 'openai/gpt-6-luna', workspace: '/workspace', tools: ['shell', 'read', 'write', 'edit', 'wait', 'history'] }),
    policy: async () => ({ instructions: 'demo', compaction_instructions: 'demo summary policy', note: 'demo policy' }),
    project: async (dir) => { const name = String(dir).split('/').filter(Boolean).pop()?.replace(/[^A-Za-z0-9_-]+/g, '-') || 'project'; return { dir, name, coordinator: `${name}.lead`, model: null, file: false }; },
    writeProject: async () => {},
    // A coordinator puts a task that edits in `~/.agent/worktrees/NAME` on branch agent/NAME.
    branch: async (dir) => { const m = /\/worktrees\/([^/]+)$/.exec(dir ?? ''); return m ? `agent/${m[1]}` : null; },
    models: async () => [{ id: 'openai/gpt-6-luna' }, { id: 'openai/gpt-6-sol' }, { id: 'anthropic/claude-sonnet-5', note: 'Claude Sonnet 5' }],
    attach: async () => {
      if (!S.bots.size) {
        // Two projects: a coordinator is a bot named `<project>.lead`, and its tasks nest under it.
        const { model } = await api.setup();
        await create('demo.lead', model);
        const t = start('demo.lead', 'what does the daemon do when a bot is busy?');
        emit({ event: 'message', bot: 'demo.lead', turn: t, data: { node: node({ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Three answers, chosen per submission: reject it, queue it behind the running turn, or steer it into that turn as a mid-flight message. The client sends the mode every time; the daemon has no default of its own.' }] }) } });
        finish('demo.lead', t);
        await create('notes.lead', model);
        const n = start('notes.lead', 'summarize the open questions in NOTES.md');
        emit({ event: 'message', bot: 'notes.lead', turn: n, data: { node: node({ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Two open questions: where worktrees live, and who runs the setup command.' }] }) } });
        finish('notes.lead', n);
        setTimeout(() => reply('demo.lead', 'ship the login fix; split the work and wait for it'), 900);
      }
      setTimeout(() => emit({ event: 'follow_live', durable: false, cursor: S.cursor }), 0);
      return { session: ++S.session };
    },
    pull: async () => {
      if (!S.queue.length) await new Promise((resolve) => { S.waiter = resolve; });
      return { events: S.queue.splice(0, 256), closed: false };
    },
    request: async (op, params = {}) => {
      switch (op) {
        case 'bots': return { bots: [...S.bots.values()].map((b) => ({ ...b })), next_after: null };
        case 'history_nodes': {
          const all = (S.lineages.get(params.bot) ?? []).filter(n => n.node <= (params.from ?? Infinity) && n.node >= (params.min_node ?? 0));
          const limit = params.limit ?? 400;
          const page = params.oldest_first ? all.slice(0,limit) : all.slice(-limit);
          const next_from = all.findLast(n=>n.node < (page[0]?.node ?? 0))?.node ?? null;
          const next_newer = all.find(n=>n.node > (page.at(-1)?.node ?? Infinity))?.node ?? null;
          return {nodes:page.slice().reverse(),next_from,next_newer};
        }
        case 'history_items': {
          const items=[];let bytes=0;
          const lineage=new Set((S.lineages.get(params.bot) ?? []).map(n=>n.node));
          if(params.nodes.some(node=>!lineage.has(node))) throw new Error('item_not_in_bot_history');
          for(const node of params.nodes) {const item=S.nodes.get(node),size=JSON.stringify(item).length*2;if(items.length && bytes+size>768*1024)break;items.push({node,item});bytes+=size;}
          return {items};
        }
        case 'resume': { const b = S.bots.get(params.bot); if (!b) throw new Error('bot_not_found'); return { ...b }; }
        case 'create': { await create(params.bot, params.model, params.created_by ?? null, null, params.workspace); return { ...S.bots.get(params.bot) }; }
        case 'submit': { const b = S.bots.get(params.bot); if (!b) throw new Error('bot_not_found'); if (b.status !== 'idle' && params.delivery === 'reject') throw new Error('bot_busy');
          if (params.delivery === 'steer' && params.expected_turn != null && params.expected_turn !== b.running_turn) throw new Error('stale_turn');
          if (b.status !== 'idle' && params.delivery === 'steer') { (b.steers ??= []).push(params.prompt); emit({ event: 'steered', bot: params.bot, turn: b.running_turn, data: {} }); return { bot: params.bot, turn: b.running_turn, status: 'steered' }; }
          if (params.workspace) b.workspace = params.workspace; // a message that names a folder moves the bot there
          const turn = S.nextTurn; reply(params.bot, params.prompt); return { bot: params.bot, turn, status: 'running', handle: `turn:${params.bot}/${turn}` }; }
        case 'interrupt': { const b = S.bots.get(params.bot); if (!b || b.running_turn === null) throw new Error('turn_not_running'); b.interrupted = true; finish(params.bot, b.running_turn, 'interrupted'); return { interrupt_requested: true }; }
        // A running source forks too, as the daemon's does from its newest finished round.
        case 'fork': { const src = S.bots.get(params.source); if (!src) throw new Error('bot_not_found'); await create(params.bot, `${src.provider}/${src.model}`, params.created_by ?? null, params.source, params.workspace ?? src.workspace, Array.isArray(params.allow) ? params.allow : src.allowed ?? null); if (Array.isArray(params.allow)) S.sides.add(params.bot); return { ...S.bots.get(params.bot) }; }
        case 'delete': { const b = S.bots.get(params.bot); if (!b) throw new Error('bot_not_found'); if (b.status !== 'idle') throw new Error('bot_busy'); b.interrupted = true; S.bots.delete(params.bot); S.lineages.delete(params.bot); S.sides.delete(params.bot); emit({ event: 'deleted', bot: params.bot, durable: false }); return { deleted: params.bot }; }
        default: throw new Error(`unsupported_in_demo:${op}`);
      }
    },
    close: () => { for (const t of S.timers) clearTimeout(t); },
  };
  return api;
})();
