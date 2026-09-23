import { spawn, spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { once } from 'node:events';
import { mkdir, writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { baseUrl, config, emit, instructions, Output, prompt } from './common.mjs';

// One headless opencode server hosts one session per agent. All native state,
// configuration and project reads are confined to synthetic private folders
// created by the runner; no user config, authentication, plugins or MCP load.
const state = process.env.AGENT_BENCH_STATE;
const workspace = process.env.AGENT_BENCH_WORKSPACE;
const settings = {
  $schema: 'https://opencode.ai/config.json',
  model: 'bench/bench-model', small_model: 'bench/bench-model',
  enabled_providers: ['bench'], default_agent: 'bench',
  autoupdate: false, share: 'disabled', snapshot: false,
  lsp: false, formatter: false, mcp: {}, plugin: [], instructions: [],
  compaction: { auto: false, prune: false },
  provider: {
    bench: {
      // @ai-sdk/openai speaks the Responses API; opencode sets store=false.
      name: 'Synthetic fixture', npm: '@ai-sdk/openai',
      options: { baseURL: baseUrl, apiKey: 'synthetic-not-a-secret' },
      models: {
        'bench-model': {
          name: 'Synthetic benchmark', tool_call: false, reasoning: false,
          attachment: false, temperature: false,
          limit: { context: 1000000, output: 32000 },
        },
      },
    },
  },
  agent: {
    bench: {
      mode: 'primary', description: 'Synthetic benchmark agent', prompt: instructions,
      // Last matching rule wins; a pattern-* deny removes every tool schema.
      permission: { '*': 'deny' },
    },
  },
};

const password = randomBytes(24).toString('base64url');
const auth = `Basic ${Buffer.from(`opencode:${password}`).toString('base64')}`;
let child;
let events;
try {
  const dirs = Object.fromEntries(['config', 'data', 'cache', 'state'].map(name => [name, `${state}/xdg-${name}`]));
  await Promise.all(Object.values(dirs).map(path => mkdir(path, { recursive: true })));
  const configPath = `${state}/opencode.json`;
  await writeFile(configPath, JSON.stringify(settings), { mode: 0o600 });
  // Captures live inside this repository. opencode searches upward for .git,
  // adopts that project and writes a project-id file into its git directory,
  // so give it an empty private repository as its project instead.
  const git = spawnSync('git', ['init', '--quiet', workspace], {
    stdio: 'ignore', env: { PATH: process.env.PATH, HOME: process.env.HOME, GIT_CONFIG_NOSYSTEM: '1' },
  });
  if (git.status !== 0) throw new Error('opencode workspace setup failed');
  // Every instance boot starts a background npm install of @opencode-ai/plugin
  // into the global config directory unless its manifest already lists it; no
  // setting disables this. Seed the manifest a prior install leaves so no
  // registry request leaves the host. --pure never loads the package itself.
  const configDir = `${dirs.config}/opencode`;
  await mkdir(`${configDir}/node_modules`, { recursive: true });
  // opencode compares package names only; the version mirrors the targets.py pin.
  const dependency = { '@opencode-ai/plugin': '1.18.32' };
  await writeFile(`${configDir}/package.json`, JSON.stringify({ dependencies: dependency }));
  await writeFile(`${configDir}/package-lock.json`, JSON.stringify({
    lockfileVersion: 3, packages: { '': { dependencies: dependency } },
  }));
  child = spawn(process.env.AGENT_BENCH_EXECUTABLE, [
    'serve', '--pure', '--hostname', '127.0.0.1', '--port', '0',
  ], {
    cwd: workspace, stdio: ['ignore', 'pipe', 'ignore'],
    env: {
      PATH: process.env.PATH, HOME: process.env.HOME,
      XDG_CONFIG_HOME: dirs.config, XDG_DATA_HOME: dirs.data,
      XDG_CACHE_HOME: dirs.cache, XDG_STATE_HOME: dirs.state,
      OPENCODE_CONFIG: configPath, OPENCODE_SERVER_PASSWORD: password,
      OPENCODE_DISABLE_AUTOUPDATE: '1', OPENCODE_DISABLE_MODELS_FETCH: '1',
      OPENCODE_DISABLE_DEFAULT_PLUGINS: '1', OPENCODE_DISABLE_CLAUDE_CODE: '1',
      OPENCODE_DISABLE_EXTERNAL_SKILLS: '1', OPENCODE_DISABLE_LSP_DOWNLOAD: '1',
      OPENCODE_DISABLE_AUTOCOMPACT: '1', OPENCODE_DISABLE_SHARE: '1',
      OPENCODE_DISABLE_PROJECT_CONFIG: '1', OPENCODE_DISABLE_EMBEDDED_WEB_UI: '1',
      OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER: '1', GIT_CONFIG_NOSYSTEM: '1',
    },
  });
  const exited = once(child, 'exit').then(() => { throw new Error('opencode exited'); });
  exited.catch(() => {});
  const lines = createInterface({ input: child.stdout });
  const listening = (async () => {
    for await (const line of lines) {
      const match = /^opencode server listening on (http:\/\/127\.0\.0\.1:\d+)$/.exec(line);
      if (match) return match[1];
    }
    throw new Error('opencode server did not start');
  })();
  const server = await Promise.race([listening, exited]);
  // Keep draining stdout so the server never blocks on a full pipe.
  (async () => { for await (const _ of lines); })().catch(() => {});

  async function call(method, path, body) {
    const response = await fetch(`${server}${path}`, {
      method, headers: { authorization: auth, 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.status === 204 ? undefined : response.json();
  }

  // Per-session active turn, fed by the server's single event stream.
  const turns = new Map();
  let failure;
  function fail(error) {
    failure ??= error;
    for (const turn of turns.values()) turn.reject(failure);
    turns.clear();
  }
  const controller = new AbortController();
  const stream = await fetch(`${server}/event`, {
    headers: { authorization: auth, accept: 'text/event-stream' }, signal: controller.signal,
  });
  if (!stream.ok) throw new Error(`HTTP ${stream.status}`);
  let connected;
  const streamReady = new Promise((resolve, reject) => { connected = { resolve, reject }; });
  events = (async () => {
    const decoder = new TextDecoder();
    let buffer = '';
    for await (const bytes of stream.body) {
      buffer += decoder.decode(bytes, { stream: true });
      let end;
      while ((end = buffer.indexOf('\n\n')) >= 0) {
        const frame = buffer.slice(0, end);
        buffer = buffer.slice(end + 2);
        const data = frame.split('\n').filter(line => line.startsWith('data:'))
          .map(line => line.slice(5).trimStart()).join('\n');
        if (!data) continue;
        const { type, properties: p } = JSON.parse(data);
        if (type === 'server.connected') { connected.resolve(); continue; }
        const active = turns.get(p?.sessionID);
        if (type === 'message.part.delta') {
          if (!active || p.field !== 'text' || !active.assistant.has(p.messageID)) {
            throw new Error('delta outside active turn');
          }
          await active.output.delta(p.delta);
          active.progress();
        } else if (type === 'message.updated') {
          if (active && p.info.role === 'assistant') active.assistant.add(p.info.id);
        } else if (type === 'message.part.updated') {
          if (p.part.type === 'tool') throw new Error('unexpected tool work');
        } else if (type === 'session.error') {
          throw new Error('unexpected engine error');
        } else if (type === 'session.status' && p.status.type === 'retry') {
          throw new Error('unexpected engine retry');
        }
      }
    }
    if (!controller.signal.aborted) throw new Error('event stream closed');
  })().catch(error => {
    if (controller.signal.aborted) return;
    connected.reject(error);
    fail(error);
  });
  await streamReady;

  const sessions = await Promise.all(Array.from({ length: config.concurrency }, (_, agent) =>
    // An explicit title means opencode's first-turn title request is skipped.
    call('POST', '/session', { title: `bench-agent-${agent}` })));
  await emit({ event: 'ready' });
  const expected = config.chunks * config.chunk_bytes;
  await Promise.all(sessions.map(async (session, agent) => {
    for (let turn = 0; turn < config.turns; turn++) {
      if (failure) throw failure;
      const output = new Output(agent, turn);
      await output.start();
      let active;
      const streamed = new Promise((resolve, reject) => {
        active = { output, reject, assistant: new Set(),
                   progress: () => { if (output.bytes === expected) resolve(); } };
        turns.set(session.id, active);
      });
      streamed.catch(() => {});
      const reply = await call('POST', `/session/${session.id}/message`, {
        agent: 'bench', parts: [{ type: 'text', text: prompt(agent, turn) }],
      });
      const text = reply.parts.filter(part => part.type === 'text').map(part => part.text).join('');
      if (reply.info.role !== 'assistant' || reply.info.error || reply.info.finish !== 'stop'
          || text !== 'x'.repeat(expected) || reply.parts.some(part => part.type === 'tool')) {
        throw new Error('opencode turn failed');
      }
      // The reply and the event stream use separate connections; wait for all deltas.
      await streamed;
      turns.delete(session.id);
      await output.end();
    }
  }));
  if (failure) throw failure;
  controller.abort();
  await events;
  const stopped = once(child, 'exit');
  child.kill('SIGTERM');
  const [code, signal] = await stopped;
  if (!(code === 0 || signal === 'SIGTERM' || code === 143)) throw new Error('opencode shutdown failed');
} catch (error) {
  // Static diagnostic categories only, written inside the ignored capture.
  const known = /^(HTTP \d{3}|opencode .+|unexpected .+|delta outside active turn|event stream closed)$/;
  await writeFile(`${state}/adapter-error.json`, JSON.stringify({
    category: known.test(error?.message ?? '') ? error.message : 'opencode adapter failed',
  })).catch(() => {});
  console.error('opencode benchmark adapter failed');
  process.exitCode = 1;
} finally {
  if (child && child.exitCode === null && child.signalCode === null) {
    const exited = once(child, 'exit');
    child.kill('SIGTERM');
    await exited;
  }
}
