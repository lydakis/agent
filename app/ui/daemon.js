// The page's only door to the daemon. Inside Tauri it is the Rust core:
// `setup`, `attach`, `request`, and `daemon` window events. In a plain
// browser it is a small simulated daemon speaking the same protocol shapes,
// so the design can be worked on without a daemon at all.
window.Daemon = (() => {
  const tauri = window.__TAURI__;
  if (tauri) {
    const { invoke } = tauri.core;
    const { listen } = tauri.event;
    const log = (m) => invoke('log', { message: String(m) }).catch(() => {});
    window.addEventListener('error', (e) => log(`error: ${e.message} @${e.filename}:${e.lineno}`));
    window.addEventListener('unhandledrejection', (e) => log(`rejection: ${e.reason?.message ?? e.reason}`));
    return {
      mode: 'live',
      log,
      setup: () => invoke('setup'),
      policy: () => invoke('policy'),
      attach: (after) => invoke('attach', { after }),
      stream: () => invoke('stream'),
      request: (op, params = {}) => invoke('request', { op, params }),
      onEvent: (cb) => listen('daemon', (e) => cb(e.payload)),
      close: () => tauri.window.getCurrentWindow().close(),
    };
  }

  // ---------- demo daemon ----------
  const S = { bots: new Map(), nodes: new Map(), nextNode: 1, nextTurn: 1, nextProc: 1, cursor: 0, listeners: [], timers: new Set() };
  const emit = (event) => { if (event.durable !== false) event.cursor = ++S.cursor; for (const cb of S.listeners) cb(event); };
  const node = (item) => { const id = S.nextNode++; S.nodes.set(id, item); return id; };
  const wait = (ms) => new Promise((r) => { const t = setTimeout(() => { S.timers.delete(t); r(); }, ms); S.timers.add(t); });
  const record = (name, model) => ({ name, status: 'idle', running_turn: null, provider: model.split('/')[0], model: model.split('/').slice(1).join('/'), workspace: '/workspace', input_tokens: 0, cached_input_tokens: 0 });

  async function create(name, model, createdBy = null) {
    if (S.bots.has(name)) throw new Error('bot_exists');
    const b = { ...record(name, model), created_by: createdBy, turns: 0, interrupted: false };
    S.bots.set(name, b);
    emit({ event: 'created', bot: name, turn: null, data: { model, created_by: createdBy } });
    return b;
  }
  async function stream(name, turn, text, pace = 40) {
    const b = S.bots.get(name);
    for (const word of text.split(' ')) {
      if (b.interrupted) return false;
      emit({ event: 'text_delta', bot: name, turn, text: word + ' ', durable: false });
      await wait(pace + Math.random() * pace);
    }
    emit({ event: 'message', bot: name, turn, data: { node: node({ type: 'message', role: 'assistant', content: [{ type: 'output_text', text }] }) } });
    return true;
  }
  async function think(name, turn, text) {
    const b = S.bots.get(name);
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
    const b = S.bots.get(name);
    if (b.interrupted) return;
    const call_id = `call_${++calls}`;
    emit({ event: 'tool_started', bot: name, turn, data: { call_id, name: tname, arguments: JSON.stringify(args), arguments_truncated: false } });
    await wait(ms);
    if (b.interrupted) return;
    emit({ event: 'tool_completed', bot: name, turn, data: { call_id, node: node({ type: 'function_call_output', call_id, output }), artifacts: [] } });
  }
  function start(name, prompt) {
    const b = S.bots.get(name);
    if (b.status !== 'idle') { emit({ event: 'queued', bot: name, turn: S.nextTurn, data: { delivery: 'queue' } }); return null; }
    const turn = S.nextTurn++;
    b.turns++; b.running_turn = turn; b.status = 'running'; b.interrupted = false;
    emit({ event: 'accepted', bot: name, turn, data: { request_id: `demo-${turn}`, node: node({ role: 'user', content: [{ type: 'input_text', text: prompt }] }), workspace: b.workspace, model: `${b.provider}/${b.model}` } });
    return turn;
  }
  function finish(name, turn, status = 'completed') {
    const b = S.bots.get(name);
    b.status = 'idle'; b.running_turn = null;
    emit({ event: 'turn_finished', bot: name, turn, data: { status, checkpoint: status === 'completed' ? S.nextNode - 1 : null, error: status === 'completed' ? null : 'cancelled', detail: null } });
  }
  async function reply(name, prompt) {
    const turn = start(name, prompt);
    if (turn === null) return;
    await wait(250);
    if (/scenario|ship|split/i.test(prompt)) { await scenario(name, turn); return; }
    if (/test|check|run/i.test(prompt)) {
      await tool(name, turn, 'shell', { command: 'cargo test -p agent-runtime' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: 'running 80 tests\ntest result: ok. 80 passed; 0 failed\n', success: true }), 900);
      await stream(name, turn, 'All green. Eighty tests pass, nothing flaky in the store or delivery suites.');
    } else if (/read|look|what|how/i.test(prompt)) {
      await tool(name, turn, 'read', { path: 'src/server/mod.rs' }, '1540 lines · dispatch at line 837', 400);
      await stream(name, turn, 'The dispatch matches each op to a store call. `wait` is the odd one out: it defers its response to the handle registry, so the session never blocks.');
    } else {
      await stream(name, turn, `Got it. ${prompt.trim().replace(/[.?!]+$/, '')}: I will keep it small and report back with a diff, not a story.`);
    }
    if (!S.bots.get(name).interrupted) finish(name, turn);
  }
  async function scenario(name, turn) {
    const m = S.bots.get(name);
    await think(name, turn, 'Three independent pieces here: plan, build, test. Peers are cheap, so spawn all three. The release build is slow and nobody depends on it until the end, so start it in the background now and collect it with the rest.');
    await stream(name, turn, 'Splitting this into three peers, kicking off the release build in the background, and waiting on all of it.');
    const proc = S.nextProc++;
    await tool(name, turn, 'shell', { command: 'cargo build --release', background: true }, JSON.stringify({ handle: `proc:${proc}`, background: true }), 300);
    const tasks = { plan: 'Write the change plan for the login fix: files, risks, tests.', build: 'Apply the login fix under src/auth and keep the diff tight.', test: 'Run the auth suite and the daemon smoke, report failures verbatim.' };
    const replies = {
      plan: 'Three files touch the session cookie. Risk is the refresh path; it needs a regression test. Plan written to PLAN.md.',
      build: 'Patched refresh_session to reissue the cookie on rotation. Two files changed, 41 lines, cargo check clean. review signed off with one nit, now a comment in the code.',
      test: 'Auth suite passes. Daemon smoke passes. One warning about an unused import in tests/auth.rs, harmless.',
    };
    const handles = [];
    for (const n of Object.keys(tasks)) {
      if (m.interrupted) return;
      const cmd = `"$AGENT_BIN" run --new --bot ${n} --model "$AGENT_MODEL" --detach '${tasks[n]}'`;
      const call_id = `call_${++calls}`;
      emit({ event: 'tool_started', bot: name, turn, data: { call_id, name: 'shell', arguments: JSON.stringify({ command: cmd }), arguments_truncated: false } });
      await wait(250);
      await create(n, `${m.provider}/${m.model}`, name);
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
    while (Object.keys(tasks).some((n) => S.bots.get(n).status !== 'idle') && !m.interrupted) await wait(200);
    if (m.interrupted) return;
    m.status = 'running';
    emit({ event: 'turn_resumed', bot: name, turn, data: { call_id: wid } });
    const results = {}; for (const h of handles) results[h] = h.startsWith('proc:') ? { exit_code: 0, stdout: 'Finished release profile in 41.2s\n', stderr: '', success: true } : { status: 'completed', text: replies[h.split(':')[1].split('/')[0]] };
    emit({ event: 'tool_completed', bot: name, turn, data: { call_id: wid, node: node({ type: 'function_call_output', call_id: wid, output: JSON.stringify({ pending: [], results }) }), artifacts: [] } });
    await wait(400);
    await stream(name, turn, 'All of it landed. Plan matches the diff, tests are green with one harmless warning, release build finished. Ready for review: two files, 41 lines.');
    finish(name, turn);
  }
  async function work(n, turn, text) {
    await wait(300);
    if (n === 'plan') { await tool(n, turn, 'read', { path: 'src/auth/' }, 'session.rs refresh.rs cookie.rs · 612 lines', 500); await tool(n, turn, 'write', { path: 'PLAN.md' }, '1.2 KiB', 600); }
    if (n === 'build') {
      await tool(n, turn, 'edit', { path: 'src/auth/session.rs' }, '+23 −8', 900);
      await tool(n, turn, 'shell', { command: 'cargo check -p auth' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: 'Finished dev profile in 2.1s\n', success: true }), 1100);
      // build asks a peer of its own to review, and waits on it: depth two.
      const cmd = `"$AGENT_BIN" run --new --bot review --model "$AGENT_MODEL" --detach 'Review the auth diff for regressions.'`;
      const call_id = `call_${++calls}`;
      emit({ event: 'tool_started', bot: n, turn, data: { call_id, name: 'shell', arguments: JSON.stringify({ command: cmd }), arguments_truncated: false } });
      await wait(250);
      const b = S.bots.get(n);
      await create('review', `${b.provider}/${b.model}`, n);
      const rt = start('review', 'Review the auth diff for regressions.');
      emit({ event: 'tool_completed', bot: n, turn, data: { call_id, node: node({ type: 'function_call_output', call_id, output: JSON.stringify({ exit_code: 0, stderr: '', stdout: JSON.stringify({ bot: 'review', handle: `turn:review/${rt}`, status: 'running', turn: rt }) + '\n', success: true }) }), artifacts: [] } });
      const wid = `call_${++calls}`;
      emit({ event: 'tool_started', bot: n, turn, data: { call_id: wid, name: 'wait', arguments: JSON.stringify({ handles: [`turn:review/${rt}`] }), arguments_truncated: false } });
      b.status = 'waiting';
      emit({ event: 'turn_waiting', bot: n, turn, data: { call_id: wid, handles: [`turn:review/${rt}`], deadline_ms: null, any: false } });
      await tool('review', rt, 'shell', { command: 'git diff --stat' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: 'src/auth/session.rs | 31 +-\nsrc/auth/refresh.rs | 10 +\n', success: true }), 500);
      await stream('review', rt, 'Diff is sound. One nit: the rotation path drops the old cookie before the new one is written; harmless today, worth a comment.', 50);
      finish('review', rt);
      b.status = 'running';
      emit({ event: 'turn_resumed', bot: n, turn, data: { call_id: wid } });
      emit({ event: 'tool_completed', bot: n, turn, data: { call_id: wid, node: node({ type: 'function_call_output', call_id: wid, output: JSON.stringify({ pending: [], results: { [`turn:review/${rt}`]: { status: 'completed', text: 'Diff is sound.' } } }) }), artifacts: [] } });
    }
    if (n === 'test') { await tool(n, turn, 'shell', { command: 'cargo test -p auth' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: 'test result: ok. 34 passed; 0 failed\n', success: true }), 1600); }
    await stream(n, turn, text, 50);
    finish(n, turn);
  }

  return {
    mode: 'demo',
    setup: async () => ({ socket: 'demo', model: 'openai/gpt-5.6-luna', workspace: '/workspace', tools: ['shell', 'read', 'write', 'edit', 'wait', 'history'] }),
    policy: async () => ({ instructions: 'demo', note: 'demo policy' }),
    attach: async () => {
      if (!S.bots.size) {
        await create('main', 'openai/gpt-5.6-luna');
        const t = start('main', 'what does the daemon do when a bot is busy?');
        emit({ event: 'message', bot: 'main', turn: t, data: { node: node({ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Three answers, chosen per submission: reject it, queue it behind the running turn, or steer it into that turn as a mid-flight message. The client sends the mode every time; the daemon has no default of its own.' }] }) } });
        finish('main', t);
        setTimeout(() => reply('main', 'ship the login fix; split the work and wait for it'), 900);
      }
      setTimeout(() => emit({ event: 'follow_live', durable: false, cursor: S.cursor }), 0);
      return { bots: [...S.bots.values()].map((b) => ({ ...b })) };
    },
    request: async (op, params = {}) => {
      switch (op) {
        case 'item': { const item = S.nodes.get(params.node); if (!item) throw new Error('item_not_in_bot_history'); return item; }
        case 'resume': { const b = S.bots.get(params.bot); if (!b) throw new Error('bot_not_found'); return { ...b }; }
        case 'create': { await create(params.bot, params.model, params.created_by ?? null); return { ...S.bots.get(params.bot) }; }
        case 'submit': { const b = S.bots.get(params.bot); if (!b) throw new Error('bot_not_found'); if (b.status !== 'idle' && params.delivery === 'reject') throw new Error('bot_busy'); reply(params.bot, params.prompt); return { bot: params.bot, turn: S.nextTurn, status: 'running', handle: `turn:${params.bot}/${S.nextTurn}` }; }
        case 'interrupt': { const b = S.bots.get(params.bot); if (!b || b.running_turn === null) throw new Error('turn_not_running'); b.interrupted = true; finish(params.bot, b.running_turn, 'interrupted'); return { interrupt_requested: true }; }
        case 'fork': { const src = S.bots.get(params.source); if (!src) throw new Error('bot_not_found'); await create(params.bot, `${src.provider}/${src.model}`); return { ...S.bots.get(params.bot) }; }
        default: throw new Error(`unsupported_in_demo:${op}`);
      }
    },
    onEvent: (cb) => { S.listeners.push(cb); return () => { S.listeners = S.listeners.filter((x) => x !== cb); }; },
    close: () => { for (const t of S.timers) clearTimeout(t); },
  };
})();
