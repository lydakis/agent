import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { createInterface } from 'node:readline';
import { writeFile } from 'node:fs/promises';
import { baseUrl, config, emit, instructions, Output, prompt } from './common.mjs';

// All native state and project reads are directed to synthetic private folders
// by the runner. No user config, authentication, hooks or plugins are reused.
const settings = {
  model: 'bench-model', model_provider: 'bench',
  model_context_window: 1000000, model_auto_compact_token_limit: 900000,
  approval_policy: 'never', sandbox_mode: 'danger-full-access',
  project_doc_max_bytes: 0, web_search: 'disabled',
  'analytics.enabled': false, 'otel.exporter': 'none',
  'otel.trace_exporter': 'none', 'otel.metrics_exporter': 'none',
  'model_providers.bench': {
    name: 'Synthetic fixture', base_url: baseUrl, wire_api: 'responses',
    requires_openai_auth: false, supports_websockets: false,
    request_max_retries: 0, stream_max_retries: 0,
  },
};
for (const feature of ['remote_models', 'responses_websockets', 'responses_websockets_v2',
  'shell_snapshot', 'shell_snapshot_v2', 'shell_tool', 'unified_exec', 'apps', 'plugins',
  'codex_hooks', 'hooks', 'memories', 'multi_agent', 'code_mode', 'code_mode_prewarm',
  'enable_request_compression']) settings[`features.${feature}`] = false;
settings['features.skip_host_skill_discovery'] = true;

function toml(value) {
  if (value && typeof value === 'object') {
    return `{ ${Object.entries(value).map(([key, v]) => `${key} = ${toml(v)}`).join(', ')} }`;
  }
  return JSON.stringify(value);
}

let child;
try {
  const args = ['app-server'];
  for (const [key, value] of Object.entries(settings)) args.push('-c', `${key}=${toml(value)}`);
  child = spawn(process.env.AGENT_BENCH_EXECUTABLE, args, {
    cwd: process.env.AGENT_BENCH_WORKSPACE, stdio: ['pipe', 'pipe', 'ignore'],
  });
  let nextId = 0;
  let failure;
  const pending = new Map();
  const turns = new Map();
  function fail(error) {
    failure = error;
    for (const { reject } of pending.values()) reject(error);
    pending.clear();
    for (const { reject } of turns.values()) reject(error);
    turns.clear();
  }
  child.on('error', fail);
  child.on('exit', () => fail(new Error('Codex exited')));
  function request(method, params) {
    if (failure) return Promise.reject(failure);
    const id = nextId++;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      child.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
    });
  }
  const reading = (async () => {
    for await (const line of createInterface({ input: child.stdout })) {
      const message = JSON.parse(line);
      if (message.id !== undefined) {
        const waiter = pending.get(message.id);
        if (!waiter || message.method) throw new Error('unexpected server request');
        pending.delete(message.id);
        if (message.error) waiter.reject(new Error(`RPC failed: ${message.error.code}`));
        else waiter.resolve(message.result);
        continue;
      }
      const p = message.params;
      const active = turns.get(p?.threadId);
      if (message.method === 'item/agentMessage/delta') {
        if (!active || (active.nativeTurn && active.nativeTurn !== p.turnId)) {
          throw new Error('delta outside active turn');
        }
        active.nativeTurn = p.turnId;
        await active.output.delta(p.delta);
      } else if (message.method === 'turn/completed') {
        if (!active || p.turn.status !== 'completed'
            || (active.nativeTurn && active.nativeTurn !== p.turn.id)) {
          throw new Error('unexpected turn completion');
        }
        await active.output.end();
        turns.delete(p.threadId);
        active.resolve();
      } else if (message.method === 'error' || message.method?.startsWith('item/tool/')) {
        throw new Error('unexpected engine error or tool work');
      }
    }
  })().catch(error => { fail(error); throw error; });
  // Attach a rejection handler immediately; the promise is awaited at shutdown.
  reading.catch(() => {});
  await request('initialize', {
    clientInfo: { name: 'agent_bench', version: '1.0.0' },
    capabilities: { experimentalApi: false },
  });
  child.stdin.write(`${JSON.stringify({ method: 'initialized' })}\n`);
  const threads = await Promise.all(Array.from({ length: config.concurrency }, () =>
    request('thread/start', {
      cwd: process.env.AGENT_BENCH_WORKSPACE, model: 'bench-model', modelProvider: 'bench',
      baseInstructions: instructions, developerInstructions: '',
      approvalPolicy: 'never', sandbox: 'danger-full-access', ephemeral: true,
      personality: 'none',
    })));
  await emit({ event: 'ready' });
  await Promise.all(threads.map(async ({ thread }, agent) => {
    for (let turn = 0; turn < config.turns; turn++) {
      const output = new Output(agent, turn);
      await output.start();
      let active;
      const completed = new Promise((resolve, reject) => {
        active = { output, resolve, reject, nativeTurn: undefined };
        turns.set(thread.id, active);
      });
      completed.catch(() => {});
      const result = await request('turn/start', {
        threadId: thread.id, input: [{ type: 'text', text: prompt(agent, turn) }],
      });
      if (active.nativeTurn && active.nativeTurn !== result.turn.id) throw new Error('turn ID mismatch');
      active.nativeTurn = result.turn.id;
      await completed;
    }
  }));
  if (failure) throw failure;
  const exited = once(child, 'exit');
  child.stdin.end();
  const [exitCode, exitSignal] = await exited;
  await reading;
  if (exitCode !== 0 || exitSignal !== null) throw new Error('Codex shutdown failed');
} catch (error) {
  // Static diagnostic categories only, written inside the ignored capture.
  await writeFile(`${process.env.AGENT_BENCH_STATE}/adapter-error.json`, JSON.stringify({
    category: error.message?.startsWith('RPC failed:') ? error.message : 'Codex adapter failed',
  }));
  console.error('Codex benchmark adapter failed');
  process.exitCode = 1;
} finally {
  if (child && child.exitCode === null && child.signalCode === null) {
    const exited = once(child, 'exit');
    child.kill('SIGTERM');
    await exited;
  }
}
