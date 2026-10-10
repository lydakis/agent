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
      policy: (workspace, profile) => invoke('policy', { workspace: workspace ?? null, profile: profile ?? null }),
      models: () => invoke('models'),
      settings: () => invoke('settings'),
      saveSettings: (changes) => invoke('save_settings', { changes }),
      restartDaemon: () => invoke('restart_daemon'),
      discoverModels: () => invoke('discover_models'),
      project: (dir) => invoke('project', { dir }),
      writeProject: ({ dir, name, model, reasoning = null, threads = null }) => invoke('write_project', { dir, name, model, reasoning, threadsModel: threads?.model ?? null, threadsReasoning: threads?.reasoning ?? null, threadsInProject: !!threads?.inProject }),
      chooseFolder: (start = null) => invoke('choose_folder', { start }),
      homeDir: () => invoke('home_dir'),
      branch: (dir) => invoke('branch', { dir }),
      readFile: (path) => invoke('read_file', { path }),
      listFiles: (dir) => invoke('list_files', { dir }),
      gitView: (dir) => invoke('git_view', { dir }),
      memoryView: (project) => invoke('memory_view', { project: project ?? null }),
      gitDiff: ({ root, path = null, from = null, untracked = false, commit = null }) => invoke('git_diff', { root, path, from, untracked, commit }),
      attach: (after) => invoke('attach', { after }),
      replaceDaemon: () => invoke('replace_daemon'),
      pull: (session) => invoke('pull', { session }),
      request: (op, params = {}) => invoke('request', { op, params }),
      // Swarms are the swarm skill's: its script, run for you, starts, adds to, stops and posts to them.
      swarms: () => invoke('swarm_run', { args: ['list'] }),
      profiles: (dir) => invoke('profiles', { dir }),
      roles: () => invoke('roles'),
      editRole: (name) => invoke('edit_role', { name }),
      triggers: (after = null) => invoke('triggers', { after }),
      trigger: (name) => invoke('trigger', { name }),
      fireTrigger: (name) => invoke('trigger_fire', { name }),
      removeTrigger: (name) => invoke('trigger_remove', { name }),
      plans: (ids) => invoke('plans', { ids }),
      forgetPlan: (id) => invoke('plan_forget', { id }),
      hosts: () => invoke('hosts'),
      openHost: (host) => invoke('open_host', { host }),
      swarmStart: ({ project, folder, goal, shared, mix, agents, budgetTokens }) => invoke('swarm_run', { args: ['start', '--project', project, '--folder', folder, '--agents', String(agents), '--budget', String(budgetTokens / 1e6), ...(shared ? [] : ['--in-project']), ...mix.flatMap((r) => ['--row', [r.model, r.share, r.identity ?? '', r.effort ?? ''].join(',')]), '--', goal] }),
      swarmAdd: (swarm) => invoke('swarm_run', { args: ['add', '--swarm', swarm] }),
      swarmStop: (swarm) => invoke('swarm_run', { args: ['stop', '--swarm', swarm] }),
      swarmBoard: (swarm, offset) => invoke('swarm_board', { swarm, offset: offset ?? null }),
      swarmPost: (swarm, text) => invoke('swarm_run', { args: ['post', '--swarm', swarm, text] }),
      swarmCheck: (swarm) => invoke('swarm_run', { args: ['check', '--swarm', swarm] }),
      close: () => tauri.window.getCurrentWindow().close(),
    };
  }

  // ---------- demo daemon ----------
  // `?first` opens as a first run: nothing connected, no model, no projects.
  const FIRST = /[?&]first\b/.test(globalThis.location?.search ?? '');
  // The settings a started daemon would get, and what each provider lists.
  const ENV = FIRST ? {} : { AGENT_PROVIDER: 'openai anthropic', OPENAI_API_KEY: 'demo', ANTHROPIC_API_KEY: 'demo' };
  const LISTS = {
    openai: [{ id: 'gpt-6-luna' }, { id: 'gpt-6-sol' }],
    anthropic: [{ id: 'claude-sonnet-5', name: 'Claude Sonnet 5' }],
    chatgpt: [{ id: 'gpt-6-luna' }],
    bedrock: [{ id: 'global.anthropic.claude-opus-5-5', name: 'Global Claude Opus 5.5' }, { id: 'global.anthropic.claude-sonnet-5', name: 'Global Claude Sonnet 5' }, { id: 'global.anthropic.claude-haiku-4-5', name: 'Global Claude Haiku 4.5' }],
    'bedrock-openai': [{ id: 'openai.gpt-6-luna' }, { id: 'qwen.qwen3-coder-480b' }],
  };
  const specs = () => (ENV.AGENT_PROVIDER ?? '').split(/\s+/).filter(Boolean);
  const listing = () => Object.fromEntries(specs().map((spec) => { const n = spec.split('=')[0]; return [n, n === 'openrouter' ? { error: 'provider_http_401', detail: 'invalid key' } : { models: LISTS[n] ?? [] }]; }));
  let listed = FIRST ? [] : null;
  const S = { swarms: new Map(), bots: new Map(), nodes: new Map(), lineages: new Map(), nextNode: 1, nextTurn: 1, nextProc: 1, nextId: 1, cursor: 0, session: 0, queue: [], waiter: null, timers: new Set(), sides: new Set(), authors: new Map(), folders: new Map() };
  // Notifications wait in a queue for the page's next pull, as the core's transport holds them.
  // A prompt another bot wrote names it with its item, as the daemon's `history_items` does.
  // Who sent a prompt, as the daemon keeps it with the node: another bot's turn, with the identity
  // its name held then, or what the client named as its origin.
  const senderOf = (by) => by?.origin ? { origin: by.origin } : by ? { from: { bot: by.bot, turn: by.turn, bot_id: S.bots.get(by.bot)?.bot_id ?? null } } : {};
  const authorOf = (event) => ({ ...(event.data.from ? { from: event.data.from } : {}), ...(event.data.origin ? { origin: event.data.origin } : {}) });
  const emit = (event) => { if (event.data?.node != null) { if (!S.lineages.has(event.bot)) S.lineages.set(event.bot, []); S.lineages.get(event.bot).push({node:event.data.node,turn:event.turn ?? null,...authorOf(event)}); } if (event.durable !== false) event.cursor = ++S.cursor; S.queue.push(event); if (S.waiter) { const w = S.waiter; S.waiter = null; w(); } };
  const node = (item) => { const id = S.nextNode++; S.nodes.set(id, item); return id; };
  const wait = (ms) => new Promise((r) => { const t = setTimeout(() => { S.timers.delete(t); r(); }, ms); S.timers.add(t); });
  const record = (name, model, reasoning = null) => ({ name, status: 'idle', running_turn: null, provider: model.split('/')[0], model: model.split('/').slice(1).join('/'), effort: reasoning, workspace: '/workspace', input_tokens: 0, cached_input_tokens: 0, tokens_used: 0 });

  async function create(name, model, createdBy = null, source = null, workspace = null, allowed = null, reasoning = null) {
    if (S.bots.has(name)) throw new Error('bot_exists');
    // Lineage is pinned to the creator's identity, and the event carries the record's list fields, as the daemon's does.
    const b = { ...record(name, model, source ? S.bots.get(source)?.effort ?? null : reasoning), ...(workspace ? { workspace } : {}), bot_id: S.nextId++, created_by: createdBy, created_by_id: createdBy ? S.bots.get(createdBy)?.bot_id ?? null : null, turns: 0, interrupted: false, ...(allowed ? { allowed } : {}) };
    S.bots.set(name, b);
    // A fork shares its source's history up to its newest finished round. The demo keeps no call
    // nodes, only their results, so that is its newest node that is not a tool result.
    if (source) {
      const all = S.lineages.get(source) ?? [];
      let end = all.length; while (end > 0 && S.nodes.get(all[end - 1].node)?.type === 'function_call_output') end--;
      S.lineages.set(name, all.slice(0, end));
    }
    const checkpoint = source ? S.lineages.get(source)?.at(-1)?.node ?? null : undefined;
    emit({ event: source ? 'forked' : 'created', bot: name, turn: null, data: { bot_id: b.bot_id, provider: b.provider, model: b.model, effort: b.effort, workspace: b.workspace, status: 'idle', running_turn: null, created_by: createdBy, created_by_id: b.created_by_id, ...(allowed ? { allowed } : {}), ...(source ? { source, checkpoint } : {}) } });
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
      const { prompt, turn: steer } = b.steers.shift();
      // The store's order: the steer's own turn finishes, then the running turn takes its message.
      emit({ event: 'turn_finished', bot: name, turn: steer, data: { status: 'steered', into: turn } });
      emit({ event: 'steered', bot: name, turn, data: { steer, node: node({ role: 'user', content: [{ type: 'input_text', text: prompt }] }), ...S.authors.get(steer) } });
      S.authors.delete(steer);
      await wait(200);
      // A board post is read and carried on from; only a person's steer gets an answer.
      if (!prompt.startsWith('[board]')) await stream(name, turn, `Noted: ${prompt.trim().replace(/[.?!]+$/, '')}. Carrying on with that in mind.`);
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
  function start(name, prompt, from = null) {
    const b = S.bots.get(name);
    if (b.status !== 'idle') { emit({ event: 'queued', bot: name, turn: S.nextTurn, data: { delivery: 'queue', ...senderOf(from) } }); return null; }
    const turn = S.nextTurn++;
    b.turns++; b.running_turn = turn; b.status = 'running'; b.interrupted = false; b.steers = []; S.folders.set(turn, b.workspace);
    emit({ event: 'accepted', bot: name, turn, data: { request_id: `demo-${turn}`, node: node({ role: 'user', content: [{ type: 'input_text', text: prompt }] }), workspace: b.workspace, model: `${b.provider}/${b.model}`, ...senderOf(from) } });
    return turn;
  }
  function finish(name, turn, status = 'completed') {
    const b = S.bots.get(name); if (!b) return;
    b.status = 'idle'; b.running_turn = null;
    emit({ event: 'turn_finished', bot: name, turn, data: { status, checkpoint: status === 'completed' ? S.nextNode - 1 : null, error: status === 'completed' ? null : 'cancelled', detail: null } });
  }
  // A steer is a turn of its own, queued until the running turn's next step takes it in.
  function steer(name, prompt, from = null) {
    const b = S.bots.get(name), turn = S.nextTurn++;
    const sender = senderOf(from); S.authors.set(turn, sender);
    (b.steers ??= []).push({ prompt, turn });
    emit({ event: 'queued', bot: name, turn, data: { delivery: 'steer', status: 'queued', ...sender } });
    return turn;
  }
  async function reply(name, prompt, from = null) {
    const turn = start(name, prompt, from);
    if (turn === null) return;
    await wait(250);
    if (name === 'home') { await home(name, turn, prompt); return; }
    if (/scenario|ship|split/i.test(prompt)) { await scenario(name, turn); return; }
    // The app telling a coordinator its tasks moved: it reads one, and passes on what another needs.
    if (prompt.startsWith('Task updates: ')) {
      const handle = /turn:[\w.-]+\/\d+/.exec(prompt)?.[0] ?? 'turn:demo.build/5';
      const wid = `call_${++calls}`;
      emit({ event: 'tool_started', bot: name, turn, data: { call_id: wid, name: 'wait', arguments: JSON.stringify({ handles: [handle], timeout_ms: 0 }), arguments_truncated: false } });
      await wait(400);
      emit({ event: 'tool_completed', bot: name, turn, data: { call_id: wid, node: node({ type: 'function_call_output', call_id: wid, output: JSON.stringify({ pending: [], results: { [handle]: { status: 'completed', text: 'Moved the cookie reissue into refresh_session, so rotate() no longer writes it. Tests that call rotate() directly now need a session.' } } }) }), artifacts: [] } });
      await tool(name, turn, 'shell', { command: `"$AGENT_BIN" run --detach --delivery queue --bot demo.test -- 'build moved the cookie reissue into refresh_session: tests calling rotate() directly now need a session first.'` }, JSON.stringify({ exit_code: 0, stderr: '', stdout: '{"bot":"demo.test","status":"queued"}\n', success: true }), 500);
      await stream(name, turn, 'Passed build\'s cookie change on to test, whose rotate() tests depend on it.');
      if (!(S.bots.get(name) ?? GONE).interrupted) finish(name, turn);
      return;
    }
    const sw = memberOf(name);
    if (sw && prompt.startsWith('You are ')) { await member(sw, name, turn); return; }
    if (sw && prompt.startsWith('[board]')) {
      const named = prompt.includes(`@${short(sw, name)}`);
      await stream(name, turn, named ? 'On it.' : 'Read it; nothing for me there.');
      if (named) await agentPost(sw, name, turn, `On it: ${prompt.replace(/^\[board\] [^:]+: /, '').replace(/@[\w.-]+\s*/g, '').split(/[.?!]/)[0]}.`);
      if (!(S.bots.get(name) ?? GONE).interrupted) finish(name, turn);
      return;
    }
    // A side chat, a fork nested under its own source, answers from the
    // history it was forked with.
    if (S.sides.has(name)) {
      await tool(name, turn, 'read', { path: 'PLAN.md' }, '1.2 KiB · three steps', 400);
      await stream(name, turn, 'Waiting on three peers: plan is done, build is waiting on its reviewer, and test is running. The release build is still going in the background.');
      if (!(S.bots.get(name) ?? GONE).interrupted) finish(name, turn);
      return;
    }
    if (/render|rich|diagram|markdown/i.test(prompt)) {
      await tool(name, turn, 'read', { path: 'src/auth/session.rs' }, '212 lines · refresh_session at line 88', 400);
      await stream(name, turn, RICH, 12);
    } else if (/test|check|run/i.test(prompt)) {
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
  const FILES = {
    'PLAN.md': ['# Login fix', '', 'The session cookie is written in three places. After this change it is written once, in [refresh_session](src/auth/session.rs).', '', '## Steps', '', '- [x] make `rotate()` pure', '- [x] write the cookie after the store commits', '- [ ] drop the reissue in `middleware.rs`', '', '## Risk', '', '> The refresh path has no regression test today. Add one before merging.', '', '```mermaid', 'flowchart LR', '    login --> rotate --> commit --> cookie', '    refresh --> rotate', '```', '', 'Latency before and after: [report/latency.vl.json](report/latency.vl.json).'].join('\n'),
    'src/auth/session.rs': ['use crate::store::{Store, SessionId};', '', '/// Rotates the token and writes the cookie once, after the store commits.', 'pub fn refresh_session(store: &Store, id: SessionId) -> Result<Cookie> {', '    let token = rotate(store, id)?; // pure: no cookie here', '    store.commit()?;', '    Ok(Cookie::new("session", token).http_only(true).secure(true))', '}', '', 'fn rotate(store: &Store, id: SessionId) -> Result<Token> {', '    let token = Token::random();', '    store.put_session(id, &token)?;', '    Ok(token)', '}', ''].join('\n'),
    'src/server/mod.rs': ['//! Dispatch: each op to its store call.', '', 'pub async fn dispatch(op: Op, store: &Store) -> Reply {', '    match op {', '        Op::Wait(handles) => registry().defer(handles).await,', '        op => store.call(op).await,', '    }', '}', ''].join('\n'),
    'report/latency.vl.json': JSON.stringify({ title: 'Refresh latency, p50 and p99 (ms)', data: { values: [['before', 'p50', 41], ['before', 'p99', 188], ['after', 'p50', 23], ['after', 'p99', 61]].map(([build, q, ms]) => ({ build, q, ms })) }, mark: 'bar', encoding: { x: { field: 'q', type: 'nominal', title: null, axis: { labelAngle: 0 } }, xOffset: { field: 'build', sort: ['before', 'after'] }, y: { field: 'ms', type: 'quantitative', title: 'ms' }, color: { field: 'build', type: 'nominal', sort: ['before', 'after'], title: null } } }, null, 2),
  };
  // Memory, as the memory skill keeps it: one fact a file, newest first in the sheet.
  const MEMORY_ROOT = '/Users/you/.agents/memory', DAY = 86400000;
  const MEMORY = {
    '': [
      { name: 'short-replies', type: 'feedback', description: 'Lead with the answer; one line when one line will do', source: 'the person, 2026-09-28', verified: '2026-10-08', modified: Date.now() - 2 * DAY },
      { name: 'paid-runs', type: 'feedback', description: 'Ask before any run that spends money on a provider', source: 'the person, 2026-09-26', verified: '2026-10-01', modified: Date.now() - 9 * DAY },
    ],
    demo: [
      { name: 'worktrees', type: 'project', description: 'Tasks that edit work in ~/.agent/worktrees, one per task', source: 'app/agents/coordinator.md', verified: '2026-10-09', modified: Date.now() - DAY },
    ],
    notes: [
      { name: 'theme', type: 'feedback', description: 'Follow the system theme; no toggle', source: 'the person, 2026-10-02', verified: '2026-10-02', modified: Date.now() - 6 * DAY },
    ],
  };
  const memoryFact = (path) => {
    for (const [scope, facts] of Object.entries(MEMORY)) for (const f of facts) {
      if (path !== `${MEMORY_ROOT}${scope ? `/projects/${scope}` : ''}/${f.name}.md`) continue;
      return { text: `---\nname: ${f.name}\ndescription: ${f.description}\ntype: ${f.type}\nsource: ${f.source}\nverified: ${f.verified}\n---\n\n${f.body ?? f.description + '.'}\n` };
    }
    return null;
  };
  // A reply that uses everything the page draws: Markdown, code, a diagram and a page preview.
  const RICH = [
    '## Session refresh', '',
    'The cookie is reissued in **one place** now, `refresh_session`, so `rotate()` stays pure. Three things changed:', '',
    '1. `rotate()` returns the new token instead of writing it', '2. `refresh_session` writes the cookie once, after the store commits', '3. the old reissue in `middleware.rs` is gone', '',
    '| path | before | after |', '|---|---|---|', '| login | 2 writes | 1 write |', '| refresh | 3 writes | 1 write |', '',
    '```rust', 'pub fn refresh_session(store: &Store, id: SessionId) -> Result<Cookie> {', '    let token = rotate(store, id)?; // pure: no cookie here', '    store.commit()?;', '    Ok(Cookie::new("session", token).http_only(true))', '}', '```', '',
    '```mermaid', 'sequenceDiagram', '    participant C as Client', '    participant S as Server', '    participant DB as Store', '    C->>S: POST /refresh', '    S->>DB: rotate(id)', '    DB-->>S: new token', '    S->>DB: commit', '    S-->>C: Set-Cookie: session', '```', '',
    'Refresh latency on the bench, before and after:', '',
    '```vega-lite', JSON.stringify({ data: { values: [['before', 'p50', 41], ['before', 'p99', 188], ['after', 'p50', 23], ['after', 'p99', 61]].map(([build, q, ms]) => ({ build, q, ms })) }, mark: 'bar', encoding: { x: { field: 'q', type: 'nominal', title: null, axis: { labelAngle: 0 } }, xOffset: { field: 'build', sort: ['before', 'after'] }, y: { field: 'ms', type: 'quantitative', title: 'ms' }, color: { field: 'build', type: 'nominal', sort: ['before', 'after'], title: null } } }), '```', '',
    'A sketch of the login card with the new copy:', '',
    '```html', '<!doctype html>', '<html><body style="margin:0;font:15px system-ui;background:#f4f1ea;display:grid;place-items:center;height:180px">', '<div style="background:#fff;padding:20px 28px;border-radius:12px;box-shadow:0 6px 24px #0002">', '<b>Signed in</b><p style="margin:6px 0 0;color:#555">Your session renews itself while you work.</p>', '</div></body></html>', '```', '',
    '> Tests that call `rotate()` directly need a session first; see [the auth notes](https://example.com/auth). The plan is in [PLAN.md](PLAN.md).',
  ].join('\n');
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
      const t = start(n, tasks[n], { bot: name, turn });
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
  // Home reads the fleet with `agent ls`, answers what it can, and hands a project's work to its lead.
  async function home(name, turn, prompt) {
    const others = [...S.bots.values()].filter((b) => b.name !== name);
    const ls = others.map((b) => JSON.stringify({ bot: b.name, status: b.status, workspace: b.workspace })).join('\n') + '\n';
    await tool(name, turn, 'shell', { command: '"$AGENT_BIN" ls' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: ls, success: true }), 400);
    if ((S.bots.get(name) ?? GONE).interrupted) return;
    const lead = others.find((b) => b.name.endsWith('.lead') && prompt.toLowerCase().includes(b.name.slice(0, -5)));
    if (lead) {
      const brief = prompt.replace(/'/g, '');
      await tool(name, turn, 'shell', { command: `"$AGENT_BIN" run --detach --delivery queue --bot ${lead.name} -- '${brief}'` }, JSON.stringify({ exit_code: 0, stderr: '', stdout: JSON.stringify({ bot: lead.name, status: 'queued' }) + '\n', success: true }), 400);
      setTimeout(() => reply(lead.name, brief, { bot: name, turn }), 300);
      await stream(name, turn, `Sent to ${lead.name.slice(0, -5)}'s lead; it reports back in its own chat.`);
    } else {
      const working = others.filter((b) => b.status !== 'idle').map((b) => b.name);
      await stream(name, turn, `Needs you: nothing right now.\n\n${working.length ? `Working: ${working.join(', ')}.` : 'Nothing is running.'} notes answered its open questions: where worktrees live, and who runs the setup command.`);
    }
    if (!(S.bots.get(name) ?? GONE).interrupted) finish(name, turn);
  }
  // A task keeps its plan with the plan skill's script; the page reads it back by bot id.
  async function plan(name, turn, ...steps) {
    const b = S.bots.get(name); if (!b || b.interrupted) return;
    const text = steps.join('\n') + '\n';
    (S.plans ??= new Map()).set(b.bot_id, text);
    await tool(name, turn, 'shell', { command: `sh "$HOME/.agents/skills/plan/plan" ${steps.map((x) => `'${x}'`).join(' ')}` }, JSON.stringify({ exit_code: 0, stderr: "", stdout: `plan saved: ${steps.length} steps\n`, success: true }), 200);
  }
  async function work(n, turn, text) {
    await wait(300);
    if (n === 'demo.plan') {
      await tool(n, turn, 'read', { path: 'src/auth/' }, 'session.rs refresh.rs cookie.rs · 612 lines', 500); await tool(n, turn, 'write', { path: 'PLAN.md' }, '1.2 KiB', 600);
      // A finding the next task would otherwise rediscover goes to the project's memory.
      const fact = { name: 'session-cookie', type: 'project', description: 'The session cookie is written in refresh_session only, after the store commits', source: `turn:${n}/${turn}`, verified: '2026-10-10', modified: Date.now(), body: 'The session cookie is written in one place, refresh_session in src/auth/session.rs, after store.commit().\n\n**Why:** writing it before the commit left a cookie for a session the store never kept.' };
      MEMORY.demo = [...MEMORY.demo.filter((f) => f.name !== fact.name), fact];
      await tool(n, turn, 'shell', { command: `"$HOME/.agent/memory" save session-cookie --type project --description '${fact.description}' --source ${fact.source} -- -` }, JSON.stringify({ exit_code: 0, stderr: '', stdout: JSON.stringify({ saved: 'session-cookie', replaced: false, duplicate: false, facts: MEMORY.demo.length }) + '\n', success: true }), 400);
    }
    if (n === 'demo.build') {
      await plan(n, turn, '[>] Make rotate() pure', '[ ] Write the cookie once, after the commit', '[ ] Check that it builds', '[ ] Get a review');
      await tool(n, turn, 'edit', { path: 'src/auth/session.rs' }, '+23 −8', 900);
      await plan(n, turn, '[x] Make rotate() pure', '[x] Write the cookie once, after the commit', '[>] Check that it builds', '[ ] Get a review');
      await tool(n, turn, 'shell', { command: 'cargo check -p auth' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: 'Finished dev profile in 2.1s\n', success: true }), 1100);
      await plan(n, turn, '[x] Make rotate() pure', '[x] Write the cookie once, after the commit', '[x] Check that it builds', '[>] Get a review');
      // build asks a peer of its own to review, and waits on it: depth two.
      if ((S.bots.get(n) ?? GONE).interrupted) return;
      const cmd = `"$AGENT_BIN" run --new --bot demo.review --model "$AGENT_MODEL" --detach 'Review the auth diff for regressions.'`;
      const call_id = `call_${++calls}`;
      emit({ event: 'tool_started', bot: n, turn, data: { call_id, name: 'shell', arguments: JSON.stringify({ command: cmd }), arguments_truncated: false } });
      await wait(250);
      const b = S.bots.get(n);
      // Created from build's shell, the reviewer works in build's worktree.
      await create('demo.review', `${b.provider}/${b.model}`, n, null, b.workspace);
      const rt = start('demo.review', 'Review the auth diff for regressions.', { bot: n, turn });
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
      await plan(n, turn, '[x] Make rotate() pure', '[x] Write the cookie once, after the commit', '[x] Check that it builds', '[x] Get a review');
    }
    if (n === 'demo.test') { await tool(n, turn, 'shell', { command: 'cargo test -p auth' }, JSON.stringify({ exit_code: 0, stderr: '', stdout: 'test result: ok. 34 passed; 0 failed\n', success: true }), 1600); }
    await stream(n, turn, text, 50);
    finish(n, turn);
  }

  // ---------- demo swarms ----------
  // A swarm's folder, kept in memory: its record and its board. The offset is a line count.
  const memberOf = (name) => [...S.swarms.values()].find((sw) => sw.members.includes(name)) ?? null;
  const short = (sw, name) => (name.startsWith(sw.project + '.') ? name.slice(sw.project.length + 1) : name);
  const swarmRecord = (sw) => ({ swarm: sw.name, dir: sw.dir, project: sw.project, goal: sw.goal, workspace: sw.workspace, budget_tokens: sw.budget, mix: sw.mix.map((r) => ({ ...r })), members: [...sw.members], ids: { ...sw.ids }, rows: { ...sw.rows }, stopped: sw.stopped });
  // Agents made from rows of the mix, each joined and then briefed, as the swarm's script does.
  async function enlist(sw, rows, each, late = false) {
    const bots = [];
    for (const [name, row] of rows) {
      const b = await api.request('create', { bot: name, model: sw.mix[row].model, effort: sw.mix[row].effort ?? null, workspace: sw.workspace, budget_tokens: each });
      sw.members.push(name); sw.ids[name] = b.bot_id; sw.rows[name] = row; bots.push(b);
      sw.made = Math.max(sw.made ?? 0, Number(name.split('-').pop()) || 0);
    }
    for (const [name] of rows) reply(name, `You are ${short(sw, name)}, one of ${sw.members.length} agents in the swarm ${short(sw, sw.name)}.${late ? ' You joined after the others started, so read the board first.' : ''}`);
    return bots;
  }
  // Who hears a post, as the swarm's script decides: whoever it names, and for your post when it
  // names nobody, everyone.
  function deliver(sw, from, text, turn = null) {
    const by = from && turn != null ? { bot: from, turn } : null;
    const named = [...text.matchAll(/@([\w.-]*\w)/g)].map((m) => m[1]);
    for (const m of sw.members) {
      const b = S.bots.get(m); if (!b || m === from) continue;
      const isNamed = named.includes(short(sw, m)) || named.includes(m);
      const prompt = `[board] ${from ? short(sw, from) : 'user'}: ${text}`;
      if (from ? !isNamed : named.length && !isNamed) continue;
      if (b.status !== 'idle') steer(m, prompt, by);
      else if (isNamed || (!from && !named.length)) reply(m, prompt, by);
    }
  }
  async function agentPost(sw, name, turn, text) {
    const b = S.bots.get(name) ?? GONE; if (b.interrupted || sw.stopped) return;
    const call_id = `call_${++calls}`;
    emit({ event: 'tool_started', bot: name, turn, data: { call_id, name: 'shell', arguments: JSON.stringify({ command: `"$SWARM" post ${JSON.stringify(text.length > 40 ? text.slice(0, 39) + '…' : text)}` }), arguments_truncated: false } });
    await wait(200);
    sw.board.push({ at: Date.now(), from: short(sw, name), bot: name, turn, text });
    b.tokens_used += 4000;
    emit({ event: 'tool_completed', bot: name, turn, data: { call_id, node: node({ type: 'function_call_output', call_id, output: JSON.stringify({ exit_code: 0, stdout: '{"posted":true}\n', stderr: '', success: true }) }), artifacts: [] } });
    deliver(sw, name, text, turn);
    await steerIn(name, turn);
  }
  // Four scripted roles; `@N` names the swarm's Nth agent, and `work` is one of the script's work commands.
  const ROLES = [
    [['post', 'Taking the profile first, so we know where p99 goes.'], ['shell', 'python3 bench/latency.py --runs 200', 'p99 142 ms · p50 61 ms', 1500], ['post', 'Profile: 61% of p99 is the TLS handshake and first byte, 22% store commits. @2 connections are yours.'], ['work', 'assign', 'conn-pool', 2, 4, 'Reuse provider connections across turns.'], ['work', 'assign', 'batch-commits', 3, 4, 'One store commit per model round.']],
    [['wait', 2600], ['work', 'claim', 'conn-pool'], ['post', 'Taking provider connection reuse. Editing src/provider/socket.rs.'], ['edit', 'src/provider/socket.rs', '+36 −12', 1700], ['shell', 'python3 bench/latency.py --runs 200', 'p99 71 ms · p50 44 ms', 1500], ['work', 'submit', 'conn-pool', 'Pooled connections: p99 142 → 71 ms on bench/latency.py.'], ['post', 'Pooled connections: p99 142 → 71 ms on bench/latency.py. @4 can you run the full suite?']],
    [['wait', 3400], ['work', 'claim', 'batch-commits'], ['post', 'Taking one store commit per model round. Editing src/store/db.rs.'], ['edit', 'src/store/db.rs', '+48 −21', 1900], ['shell', 'cargo test -p agent-runtime store', 'test result: ok. 64 passed; 0 failed', 1300], ['post', 'Batched commits are in; the 64 store tests pass.']],
    [['wait', 1800], ['read', 'board.jsonl'], ['post', 'Keeping cargo test green: I will run the suite as changes land.'], ['shell', 'cargo test', 'test result: ok. 212 passed; 0 failed', 2200], ['wait', 2800], ['work', 'review', 'conn-pool', 'supported', 'Full suite passes with pooling; p99 reproduced at 72 ms.'], ['post', 'Full suite after both changes: 212 passed.']],
  ];
  // A work command's board line and what it changes, as the script folds it.
  function swarmWork(sw, name, turn, [kind, task, ...a]) {
    const me = short(sw, name), base = me.replace(/-\d+$/, ''), t = sw.state.tasks[task];
    const line = { at: Date.now(), from: me, bot: name, turn, kind, stream: task };
    if (kind === 'assign') { const [owner, reviewer] = [`${base}-${a[0]}`, `${base}-${a[1]}`]; sw.state.tasks[task] = { owner, reviewer, brief: a[2], status: 'assigned' }; line.text = `${owner} owns it, ${reviewer} reviews: ${a[2]}`; }
    else if (kind === 'claim') { t.status = 'working'; sw.state.streams[me] = task; line.text = 'claimed'; }
    else if (kind === 'submit') { Object.assign(t, { status: 'submitted', result: a[0] }); delete sw.state.streams[me]; line.text = a[0]; }
    else { Object.assign(t, { status: 'reviewed', verdict: a[0], evidence: a[1] }); line.text = `${a[0]}: ${a[1]}`; }
    sw.board.push(line);
  }
  async function member(sw, name, turn) {
    const i = sw.members.indexOf(name), mine = ROLES[i % ROLES.length], b = S.bots.get(name);
    const base = short(sw, sw.members[0]).replace(/-\d+$/, '');
    await think(name, turn, 'Read the goal and the board first, then take a piece nobody holds.');
    for (const [op, ...a] of mine) {
      if (b.interrupted) return;
      if (op === 'wait') await wait(a[0]);
      else if (op === 'post') await agentPost(sw, name, turn, a[0].replace(/@(\d)/g, (_, n) => `@${base}-${n}`));
      else if (op === 'read') await tool(name, turn, 'read', { path: `${sw.dir}/${a[0]}` }, `${sw.board.length} posts`, 400);
      else if (op === 'work') { if (!sw.stopped) await tool(name, turn, 'shell', { command: `"$SWARM" ${a[0]} ${a[1]}` }, JSON.stringify({ exit_code: 0, stderr: '', stdout: '{"task":"' + a[1] + '"}\n', success: true }), 300).then(() => swarmWork(sw, name, turn, a)); }
      else if (op === 'edit') await tool(name, turn, 'edit', { path: a[0] }, a[1], a[2]);
      else await tool(name, turn, 'shell', { command: a[0] }, JSON.stringify({ exit_code: 0, stderr: '', stdout: a[1] + '\n', success: true }), a[2]);
      b.tokens_used += 20000;
    }
    await stream(name, turn, 'My piece is done and posted.', 30);
    if (!b.interrupted) finish(name, turn);
  }

  const api = {
    setup: async () => ({ socket: 'demo', host: null, workspace: '/workspace', tools: ['shell', 'read', 'write', 'edit', 'wait', 'history'] }),
    hosts: async () => [{ alias: 'box', to: { user: 'you', hostname: 'box.example', port: '22' } }],
    openHost: async () => { throw new Error('demo mode opens no windows'); },
    settings: async () => ({ providers: specs(), region: ENV.AWS_REGION ?? null, profile: ENV.AWS_PROFILE ?? null, keys: ['ANTHROPIC_API_KEY', 'OPENAI_API_KEY', 'OPENROUTER_API_KEY', 'AWS_BEARER_TOKEN_BEDROCK'].filter((k) => ENV[k]) }),
    saveSettings: async (changes) => { for (const [k, v] of Object.entries(changes)) { if (v) ENV[k] = v; else delete ENV[k]; } },
    restartDaemon: async () => { await wait(400); },
    discoverModels: async () => {
      await wait(700);
      const answer = listing();
      const found = Object.entries(answer).flatMap(([n, l]) => (l.models ?? []).map((m) => ({ id: `${n}/${m.id}`, ...(m.name ? { note: m.name } : {}) })));
      if (found.length) listed = found;
      return { providers: Object.fromEntries(Object.entries(answer).map(([n, l]) => [n, l.models ? { models: l.models.length } : l])), written: !!found.length, error: found.length ? null : 'models_none_listed' };
    },
    policy: async () => ({ instructions: 'demo', compaction_instructions: 'demo summary policy', note: 'demo policy' }),
    project: async (dir) => { const name = String(dir).split('/').filter(Boolean).pop()?.replace(/[^A-Za-z0-9_-]+/g, '-') || 'project'; return { dir, name, coordinator: `${name}.lead`, model: null, file: false }; },
    writeProject: async () => {},
    chooseFolder: async () => '/Users/you/Developer/weather',
    homeDir: async () => '/Users/you',
    // A coordinator puts a task that edits in `~/.agent/worktrees/NAME` on branch agent/NAME.
    // The demo's files, by their path under any agent's folder.
    readFile: async (path) => {
      const fact = memoryFact(path); if (fact) return new TextEncoder().encode(fact.text).buffer;
      const hit = Object.keys(FILES).find((k) => path === k || path.endsWith(`/${k}`));
      if (hit == null) throw new Error(path.endsWith('/') ? `${path}: is a folder` : `${path}: no such file`);
      return new TextEncoder().encode(FILES[hit]).buffer;
    },
    // The demo's repository is its files, under whichever folder asks.
    listFiles: async (dir) => ({ root: dir, files: Object.keys(FILES).sort(), more: false }),
    branch: async (dir) => { const m = /\/worktrees\/([^/]+)$/.exec(dir ?? ''); return m ? `agent/${m[1]}` : null; },
    // The demo's memory: yours, and the demo project's, which a task adds to as it works.
    memoryView: async (project) => {
      const scope = (name) => { const d = name ? `${MEMORY_ROOT}/projects/${name}` : MEMORY_ROOT; return { name, dir: d, facts: (MEMORY[name ?? ''] ?? []).map((f) => ({ ...f, path: `${d}/${f.name}.md` })) }; };
      return { user: scope(null), projects: project == null ? ['demo', 'notes'].map(scope) : [scope(project)], more: 0 };
    },
    // The demo's repository: the project folder on main and each task's worktree on its branch, with
    // the changes a task that edits leaves before it commits.
    gitView: async (dir) => {
      const m = /\/worktrees\/([^/]+)$/.exec(dir ?? ''), trees = [...S.bots.values()].map((b) => b.workspace).filter((w) => /\/worktrees\//.test(w ?? ''));
      return {
        root: dir, branch: m ? `agent/${m[1]}` : 'main...origin/main',
        changes: m ? [{ code: ' M', path: 'src/auth/session.rs', from: null }, { code: 'M ', path: 'src/server/mod.rs', from: null }, { code: '??', path: 'report/latency.vl.json', from: null }] : [],
        more: false,
        commits: [
          ...(m ? [{ sha: 'c41d9e2f7a0b3c5d6e7f8091a2b3c4d5e6f70812', subject: 'Write the session cookie once, after the store commits', author: m[1], when: '4 minutes ago' }] : []),
          { sha: '8a1f03b6c2d4e5f60718293a4b5c6d7e8f901234', subject: 'Plan the login fix', author: 'you', when: '2 hours ago' },
          { sha: '3e9b77d0a1b2c3d4e5f60718293a4b5c6d7e8f90', subject: 'Add the session store', author: 'you', when: '3 days ago' },
        ],
        worktrees: [{ path: '/workspace', branch: 'main' }, ...[...new Set(trees)].map((w) => ({ path: w, branch: `agent/${w.split('/').pop()}` }))],
      };
    },
    gitDiff: async ({ path, commit }) => {
      await wait(60);
      const session = ['@@ -1,8 +1,9 @@', ' use crate::store::{Store, SessionId};', ' ', '-/// Rotates the token and reissues the cookie.', '+/// Rotates the token and writes the cookie once, after the store commits.', ' pub fn refresh_session(store: &Store, id: SessionId) -> Result<Cookie> {', '-    let token = rotate_and_set_cookie(store, id)?;', '+    let token = rotate(store, id)?; // pure: no cookie here', '+    store.commit()?;', '     Ok(Cookie::new("session", token).http_only(true).secure(true))', ' }', ' '];
      const server = ['@@ -3,6 +3,7 @@', ' pub async fn dispatch(op: Op, store: &Store) -> Reply {', '     match op {', '         Op::Wait(handles) => registry().defer(handles).await,', '+        Op::Refresh(id) => refresh_session(store, id).into(),', '         op => store.call(op).await,', '     }', ' }'];
      const file = (p, hunk, mode = '') => [`diff --git a/${p} b/${p}`, ...(mode ? [mode] : []), 'index 1111111..2222222 100644', mode ? '--- /dev/null' : `--- a/${p}`, `+++ b/${p}`, ...hunk];
      if (commit) return { text: [...file('src/auth/session.rs', session), ...file('src/server/mod.rs', server)].join('\n') + '\n', cut: false };
      if (path === 'src/server/mod.rs') return { text: file(path, server).join('\n') + '\n', cut: false };
      if (path === 'src/auth/session.rs') return { text: file(path, session).join('\n') + '\n', cut: false };
      const lines = (FILES[path] ?? '').replace(/\n$/, '').split('\n');
      return { text: file(path, [`@@ -0,0 +1,${lines.length} @@`, ...lines.map((l) => `+${l}`)], 'new file mode 100644').join('\n') + '\n', cut: false };
    },
    swarms: async () => ({ swarms: [...S.swarms.values()].map(swarmRecord), broken: [] }),
    // The demo has no editor to open: Edit only says your copy is now the one read.
    roles: async () => ['coordinator', 'home'].map(name => ({ name, file: S.ownRoles?.has(name) ? `/home/you/.agents/agents/${name}.md` : null })),
    editRole: async (name) => { (S.ownRoles ??= new Set()).add(name); return `/home/you/.agents/agents/${name}.md`; },
    // Triggers a coordinator made: a task that checks its PR, a reviewer started at a commit that changes src
    // whose answer goes to the lead, a weekday digest, and one whose agent was deleted before its time came.
    triggers: async () => ({ triggers: (S.triggers ??= [
      { name: 'demo.build', bot: 'demo.build', bot_id: 4, when: 'every 30m', once: false, sent: 4, message: "Check the login PR: fix a red CI run and answer new review comments. When it is merged, remove this trigger.", last: { outcome: 'sent', turn: 4, fired_ms: Date.now() - 12 * 60000 } },
      { name: 'demo.review', bot: 'demo.review', bot_id: null, start: { model: 'anthropic/claude-sonnet-5', effort: null }, reply_to: 'demo.lead', if: '! git diff --quiet HEAD~1 -- src', when: 'commit /Users/you/demo', once: false, runs: 3, sent: 0, message: 'Review the newest commit for regressions; say what you found.', last: null },
      { name: 'demo.docs', bot: 'demo.docs', bot_id: 9, when: 'in 2h', once: true, ended: true, message: 'Check whether the docs preview deployed.', last: { outcome: 'gone', fired_ms: Date.now() - 95 * 60000 } },
      { name: 'notes.digest', bot: 'notes.lead', bot_id: 2, when: 'cron 0 9 * * 1-5', once: false, sent: 6, message: 'Summarize what changed in NOTES.md since yesterday, and list the questions still open.', last: { outcome: 'sent', turn: 6, fired_ms: Date.now() - 20 * 3600000 } },
    ]).map((x) => ({ ...x })), next_after: null }),
    trigger: async (name) => { const x = (S.triggers ?? []).find((t) => t.name === name); return x ? { ...x } : null; },
    // A fire sends its message after the line saying which trigger, when, and why, as the trigger script does.
    fireTrigger: async (name) => {
      const x = (S.triggers ?? []).find((t) => t.name === name); if (!x) throw new Error(`trigger_not_found: ${name}`);
      const d = new Date(), p = (n) => String(n).padStart(2, '0');
      if (S.bots.has(x.bot)) setTimeout(() => reply(x.bot, `[trigger ${name} · ${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())} · fired]\n${x.message}`, { origin: 'trigger' }), 300);
      setTimeout(() => { x.last = { outcome: 'sent', turn: 1, fired_ms: Date.now() }; x.sent = (x.sent ?? 0) + 1; }, 600);
      return { name, fired: true };
    },
    removeTrigger: async (name) => { S.triggers = (S.triggers ?? []).filter((x) => x.name !== name); },
    plans: async (ids) => Object.fromEntries(ids.filter((id) => S.plans?.has(id)).map((id) => [id, S.plans.get(id)])),
    forgetPlan: async (id) => { S.plans?.delete(id); },
    profiles: async () => [{ name: 'reviewer', summary: 'Reviews changes and reports bugs only', model: 'anthropic/claude-sonnet-5' }, { name: 'tester', summary: 'Keeps the test suite green', model: null }],
    // Named from the goal's longest word and dealt as the swarm's script does it.
    swarmStart: async ({ project, folder, goal, shared, mix, agents, budgetTokens }) => {
      const word = (goal.toLowerCase().match(/[a-z0-9]{3,}/g) ?? []).slice(0, 6).reduce((a, w) => (w.length > a.length ? w : a), '') || 'swarm';
      let full = `${project}.${word}`; for (let k = 2; S.swarms.has(full); k++) full = `${project}.${word}-${k}`;
      const counts = mix.map(() => 0), rows = [];
      for (let t = 1; t <= agents; t++) { let best = 0; mix.forEach((m, i) => { if (m.share * t - 100 * counts[i] > mix[best].share * t - 100 * counts[best]) best = i; }); counts[best] += 1; rows.push(best); }
      const sw = { name: full, project, goal, mix, budget: budgetTokens, state: { roles: {}, streams: {}, tasks: {} }, dir: `~/.agent/swarms/${full}`, workspace: shared ? `~/.agent/worktrees/${full}` : folder, members: [], ids: {}, rows: {}, stopped: false, board: [{ at: Date.now(), from: 'user', text: goal }] };
      S.swarms.set(full, sw); await wait(300);
      const bots = await enlist(sw, rows.map((row, i) => [`${full}-${i + 1}`, row]), Math.max(1, Math.floor(budgetTokens / rows.length)));
      return { swarm: swarmRecord(sw), bots, failed: [] };
    },
    swarmAdd: async (swarm) => {
      const sw = S.swarms.get(swarm), each = Math.max(1, Math.floor(sw.budget / Math.max(1, sw.members.length)));
      // The row furthest below its share among the members still there, as the script picks it.
      const live = sw.members.filter((m) => S.bots.get(m)?.id === sw.ids[m]), t = live.length + 1;
      const counts = sw.mix.map((_, r) => live.filter((m) => sw.rows[m] === r).length);
      const row = counts.reduce((best, c, r) => (sw.mix[r].share * t - 100 * c > sw.mix[best].share * t - 100 * counts[best] ? r : best), 0);
      // A number no agent of the swarm ever had, as the swarm's script does.
      let i = (sw.made ?? 0) + 1; while (S.bots.has(`${swarm}-${i}`)) i++;
      const bots = await enlist(sw, [[`${swarm}-${i}`, row]], each, true); sw.budget += each;
      return { swarm: swarmRecord(sw), bots, failed: [] };
    },
    swarmStop: async (swarm) => {
      const sw = S.swarms.get(swarm); sw.stopped = true;
      for (const m of sw.members) { const b = S.bots.get(m); if (b?.running_turn != null) { b.interrupted = true; finish(m, b.running_turn, 'interrupted'); } }
      return { swarm: swarmRecord(sw), failed: [] };
    },
    swarmBoard: async (swarm, offset) => { const sw = S.swarms.get(swarm); const from = offset ?? Math.max(0, sw.board.length - 500); return { lines: sw.board.slice(from), offset: sw.board.length, more: false, reset: offset == null, state: JSON.parse(JSON.stringify(sw.state)) }; },
    swarmPost: async (swarm, text) => {
      const sw = S.swarms.get(swarm); sw.stopped = false; sw.board.push({ at: Date.now(), from: 'user', text });
      const busy = sw.members.filter((m) => S.bots.get(m)?.status !== 'idle');
      deliver(sw, null, text);
      return { posted: true, steered: busy.map((m) => short(sw, m)), woke: [], missed: [] };
    },
    models: async () => listed ?? [{ id: 'openai/gpt-6-luna' }, { id: 'openai/gpt-6-sol' }, { id: 'anthropic/claude-sonnet-5', note: 'Claude Sonnet 5' }],
    attach: async () => {
      if (!specs().length) throw new Error('no_provider: connect a provider in Settings');
      if (!S.bots.size && !FIRST) {
        // Two projects: a coordinator is a bot named `<project>.lead`, and its tasks nest under it.
        const model = 'openai/gpt-6-luna';
        await create('demo.lead', model, null, null, null, null, 'high');
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
      return { session: ++S.session, store: 'demo', workspace: '/workspace' };
    },
    pull: async () => {
      if (!S.queue.length) await new Promise((resolve) => { S.waiter = resolve; });
      return { events: S.queue.splice(0, 256), closed: false };
    },
    request: async (op, params = {}) => {
      switch (op) {
        case 'provider_models': await wait(500); return { providers: listing() };
        // In name order after `after`, as the daemon pages them.
        case 'bots': return { bots: [...S.bots.values()].filter((b) => params.after == null || b.name > params.after).sort((a, c) => (a.name < c.name ? -1 : 1)).map((b) => ({ ...b })), next_after: null };
        case 'history_nodes': {
          const all = (S.lineages.get(params.bot) ?? []).filter(n => n.node <= (params.from ?? Infinity) && n.node >= (params.min_node ?? 0));
          const limit = params.limit ?? 400;
          const page = params.oldest_first ? all.slice(0,limit) : all.slice(-limit);
          const next_from = all.findLast(n=>n.node < (page[0]?.node ?? 0))?.node ?? null;
          const next_newer = all.find(n=>n.node > (page.at(-1)?.node ?? Infinity))?.node ?? null;
          const workspaces = [];
          for (const turn of [...new Set(page.map(n=>n.turn))].sort((a,b)=>a-b)) {
            const folder = S.folders.get(turn); if (folder == null) continue;
            const group = workspaces.find(w=>w.folder===folder);
            if (group) group.turns.push(turn); else workspaces.push({folder, turns:[turn]});
          }
          return {nodes:page.slice().reverse(),next_from,next_newer,workspaces};
        }
        case 'history_items': {
          const items=[];let bytes=0;
          const lineage=new Set((S.lineages.get(params.bot) ?? []).map(n=>n.node));
          if(params.nodes.some(node=>!lineage.has(node))) throw new Error('item_not_in_bot_history');
          const sent=new Map((S.lineages.get(params.bot) ?? []).map(({node,from,origin})=>[node,{...(from?{from}:{}),...(origin?{origin}:{})}]));
          for(const node of params.nodes) {const item=S.nodes.get(node),size=JSON.stringify(item).length*2;if(items.length && bytes+size>768*1024)break;items.push({node,item,...sent.get(node)});bytes+=size;}
          return {items};
        }
        // A demo bot's only unfinished turn is the one it runs.
        case 'turns': { const b = S.bots.get(params.bot); if (!b) throw new Error('bot_not_found'); return { turns: b.running_turn != null && params.after < b.running_turn ? [{ turn: b.running_turn, status: b.status }] : [], next_after: null }; }
        case 'resume': { const b = S.bots.get(params.bot); if (!b) throw new Error('bot_not_found'); return { ...b }; }
        case 'create': { await create(params.bot, params.model, params.created_by ?? null, null, params.workspace, null, params.effort ?? null); const b = S.bots.get(params.bot); if (params.budget_tokens) b.budget_tokens = params.budget_tokens; return { ...b }; }
        case 'submit': { const b = S.bots.get(params.bot); if (!b) throw new Error('bot_not_found'); if (b.status !== 'idle' && params.delivery === 'reject') throw new Error('bot_busy');
          if (params.delivery === 'steer' && params.expected_turn != null && params.expected_turn !== b.running_turn) throw new Error('stale_turn');
          const by = params.from ?? (params.origin ? { origin: params.origin } : null);
          if (b.status !== 'idle' && params.delivery === 'steer') { const turn = steer(params.bot, params.prompt, by); return { bot: params.bot, turn, status: 'queued' }; }
          if (params.workspace) b.workspace = params.workspace; // a message that names a folder moves the bot there
          const turn = S.nextTurn; reply(params.bot, params.prompt, by); return { bot: params.bot, turn, status: 'running', handle: `turn:${params.bot}/${turn}` }; }
        case 'interrupt': { const b = S.bots.get(params.bot); if (!b || b.running_turn === null) throw new Error('turn_not_running'); b.interrupted = true; finish(params.bot, b.running_turn, 'interrupted'); return { interrupt_requested: true }; }
        // A running source forks too, as the daemon's does from its newest finished round.
        case 'fork': { const src = S.bots.get(params.source); if (!src) throw new Error('bot_not_found'); await create(params.bot, `${src.provider}/${src.model}`, params.created_by ?? null, params.source, params.workspace ?? src.workspace, Array.isArray(params.allow) ? params.allow : src.allowed ?? null); if (params.created_by === params.source) S.sides.add(params.bot); return { ...S.bots.get(params.bot) }; }
        case 'delete': { const b = S.bots.get(params.bot); if (!b) throw new Error('bot_not_found'); if (b.status !== 'idle') throw new Error('bot_busy'); b.interrupted = true; S.bots.delete(params.bot); S.lineages.delete(params.bot); S.sides.delete(params.bot); emit({ event: 'deleted', bot: params.bot, durable: false }); return { deleted: params.bot }; }
        default: throw new Error(`unsupported_in_demo:${op}`);
      }
    },
    close: () => { for (const t of S.timers) clearTimeout(t); },
  };
  return api;
})();
