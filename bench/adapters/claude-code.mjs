import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { createInterface } from 'node:readline';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { config, emit, instructions, Output, port, prompt } from './common.mjs';

// One native Claude Code process per agent, as the CLI and the Agent SDK deploy
// it, each driven over its stream-json stdio protocol for all of that agent's
// turns. The runner supplies synthetic private HOME and CLAUDE_CONFIG_DIR
// folders; no user settings, credentials, hooks or plugins are read.
const env = Object.fromEntries(Object.entries(process.env)
  .filter(([key]) => !key.startsWith('AGENT_BENCH_')));
Object.assign(env, {
  ANTHROPIC_BASE_URL: `http://127.0.0.1:${port}`,
  ANTHROPIC_API_KEY: 'synthetic-not-a-secret',
  // Documented switches for optional traffic and behavior. Disabled work is
  // not proven unallocated.
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1', DISABLE_TELEMETRY: '1',
  DISABLE_ERROR_REPORTING: '1', DISABLE_UPDATES: '1', DISABLE_AUTO_COMPACT: '1',
  CLAUDE_CODE_MAX_RETRIES: '0', CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK: '1',
});
const args = [
  '-p', '--bare', '--input-format', 'stream-json', '--output-format', 'stream-json',
  '--verbose', '--include-partial-messages', '--model', 'bench-model', '--tools', '',
  '--strict-mcp-config', '--system-prompt', instructions, '--no-session-persistence',
  '--permission-prompts', 'none',
];

class Session {
  constructor(agent) {
    this.agent = agent;
    this.active = undefined;
    this.sessionId = undefined;
    this.child = spawn(process.env.AGENT_BENCH_EXECUTABLE, args, {
      cwd: workspace, env, stdio: ['pipe', 'pipe', 'ignore'],
    });
    this.exited = once(this.child, 'exit');
    this.exited.catch(() => {});
    this.failure = undefined;
    this.child.on('error', error => this.fail(error));
    this.child.on('exit', () => this.fail(new Error('Claude Code exited')));
    this.child.stdin.on('error', error => this.fail(error));
    this.initialized = new Promise((resolve, reject) => { this.init = { resolve, reject }; });
    this.initialized.catch(() => {});
    this.reading = this.read().catch(error => { this.fail(error); throw error; });
    this.reading.catch(() => {});
  }

  fail(error) {
    this.failure ??= error;
    this.init.reject(this.failure);
    this.active?.reject(this.failure);
  }

  write(message) {
    if (this.failure) throw this.failure;
    this.child.stdin.write(`${JSON.stringify(message)}\n`);
  }

  async read() {
    for await (const line of createInterface({ input: this.child.stdout })) {
      const message = JSON.parse(line);
      const active = this.active;
      if (message.type === 'control_response') {
        const { response } = message;
        if (response?.request_id !== 'init' || response.subtype !== 'success') {
          throw new Error('initialize failed');
        }
        this.init.resolve();
      } else if (message.type === 'system' && message.subtype === 'init') {
        if (!active || !Array.isArray(message.tools) || message.tools.length
            || (this.sessionId && this.sessionId !== message.session_id)) {
          throw new Error('unexpected session or tools');
        }
        this.sessionId = message.session_id;
      } else if (message.type === 'stream_event') {
        const event = message.event;
        if (!active || message.parent_tool_use_id) throw new Error('event outside active turn');
        if (event.type === 'content_block_start' && event.content_block?.type !== 'text') {
          throw new Error('unexpected non-text content');
        }
        if (event.type === 'content_block_delta') {
          if (event.delta?.type !== 'text_delta') throw new Error('unexpected non-text delta');
          await active.output.delta(event.delta.text);
        }
      } else if (message.type === 'assistant') {
        if (!active || message.message?.content?.some(block => block.type !== 'text')) {
          throw new Error('unexpected assistant content');
        }
      } else if (message.type === 'result') {
        if (!active || message.subtype !== 'success' || message.is_error
            || message.stop_reason !== 'end_turn' || message.session_id !== this.sessionId) {
          throw new Error('unexpected turn result');
        }
        this.active = undefined;
        await active.output.end();
        active.resolve();
      } else if (message.type === 'user' || message.type === 'control_request'
                 || message.type === 'tool_progress') {
        throw new Error('unexpected tool or permission work');
      }
      // Remaining informational messages (status updates) carry no workload data.
    }
  }

  async turn(turn) {
    const output = new Output(this.agent, turn);
    await output.start();
    const completed = new Promise((resolve, reject) => {
      this.active = { output, resolve, reject };
    });
    completed.catch(() => {});
    if (this.failure) this.active.reject(this.failure);
    this.write({ type: 'user', message: { role: 'user', content: prompt(this.agent, turn) },
                 parent_tool_use_id: null });
    await completed;
  }

  async close() {
    await this.initialized;
    this.child.stdin.end();
    const [exitCode, exitSignal] = await this.exited;
    await this.reading;
    if (exitCode !== 0 || exitSignal !== null) throw new Error('Claude Code shutdown failed');
  }
}

// The CLI discovers the enclosing repository from its working directory with
// its own file walk, then runs git status and log there. Captures live inside
// this repository, so each run uses an empty directory outside it instead.
let workspace;
const sessions = [];
try {
  workspace = await mkdtemp(join(tmpdir(), 'agent-bench-claude-'));
  for (let agent = 0; agent < config.concurrency; agent++) sessions.push(new Session(agent));
  // The Agent SDK's initialize handshake: every process is started and serving.
  for (const session of sessions) {
    session.write({ type: 'control_request', request_id: 'init', request: { subtype: 'initialize' } });
  }
  await Promise.all(sessions.map(session => session.initialized));
  await emit({ event: 'ready' });
  await Promise.all(sessions.map(async session => {
    for (let turn = 0; turn < config.turns; turn++) await session.turn(turn);
    await session.close();
  }));
} catch (error) {
  // Static diagnostic categories only, written inside the ignored capture.
  await writeFile(`${process.env.AGENT_BENCH_STATE}/adapter-error.json`, JSON.stringify({
    category: 'Claude Code adapter failed',
  }));
  console.error('Claude Code benchmark adapter failed');
  process.exitCode = 1;
} finally {
  await Promise.all(sessions.map(async ({ child }) => {
    if (child.exitCode === null && child.signalCode === null) {
      const exited = once(child, 'exit');
      child.kill('SIGTERM');
      await exited;
    }
  }));
  if (workspace) await rm(workspace, { recursive: true, force: true });
}
