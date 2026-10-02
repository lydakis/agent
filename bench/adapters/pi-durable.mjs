// Pi Durable lifecycle adapter: one Harness over one SQLite store, one
// conversation per named bot, driven by the observer over stdio JSON lines.
// It answers the same lifecycle steps the Rust daemon's stdio protocol does
// (create, submit, resume, re-read, item read, fork); see BENCHMARKS.md.
import { once } from 'node:events';
import { createInterface } from 'node:readline';
import { BACKGROUND_CONTEXT as context } from '@earendil-works/chord/context';
import { createRegistry, defineDocFamily, defineExtension, defineTool, Harness, watchEvents } from '@earendil-works/pi-durable';
import { NodeExecutionEnv } from '@earendil-works/pi-durable/env/node';
import { SqliteStorage } from '@earendil-works/pi-durable/storage/sqlite';
import { openNodeSqliteDatabase } from '@earendil-works/pi-durable/storage/sqlite/node';
import { createBashTool, createEditTool, createReadTool, createWriteTool } from '@earendil-works/pi-durable/tools';
import { createModels, createProvider, Type } from 'pi-ai-durable';
import { openAIResponsesApi } from 'pi-ai-durable/api/openai-responses.lazy';

const port = Number(process.env.AGENT_BENCH_PORT);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid fixture port');
const store = process.env.AGENT_BENCH_STORE;
const synchronous = process.env.AGENT_BENCH_SYNCHRONOUS;
const tools = (process.env.AGENT_BENCH_TOOLS ?? '').split(',').filter(Boolean);
if (!store || !['full', 'normal'].includes(synchronous)) throw new Error('store and synchronous mode required');
const baseUrl = `http://127.0.0.1:${port}/v1`;
// SQLite reports synchronous as 1 (NORMAL) or 2 (FULL).
const SYNCHRONOUS = { normal: 1, full: 2 };

const model = {
  id: 'synthetic-model', name: 'Synthetic benchmark', api: 'openai-responses',
  provider: 'bench', baseUrl, reasoning: false, input: ['text'],
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 1000000, maxTokens: 1000000,
};
const models = createModels();
models.setProvider(createProvider({
  id: 'bench', name: 'Synthetic benchmark', baseUrl,
  auth: { apiKey: { name: 'Synthetic', resolve: async () => ({ auth: { apiKey: 'synthetic-not-a-secret' } }) } },
  models: [model], api: openAIResponsesApi(),
}));

// The tools the Rust lifecycle can register: echo returns its text, and Pi's
// own bash, read, write and edit tools stand in for Agent's shell and file tools.
const echo = defineTool({
  name: 'echo', description: 'Return the given text.',
  parameters: Type.Object({ text: Type.String() }),
  execute: async args => ({ content: [{ type: 'text', text: args.text }] }),
});
const available = { echo, shell: createBashTool(), read: createReadTool(), write: createWriteTool(),
                    edit: createEditTool() };
if (tools.some(name => !available[name])) throw new Error('unknown tool');
const registry = createRegistry();
registry.install(defineExtension({ name: 'bench', tools: tools.map(name => available[name]) }));

// Bot name -> conversation, written in the commit that creates the conversation.
const Bot = defineDocFamily({ kind: 'bench.bot', version: 1, scope: 'session', family: true,
                              initial: conversation => ({ conversation }) });

const emit = async value => {
  if (!process.stdout.write(`${JSON.stringify(value)}\n`)) await once(process.stdout, 'drain');
};

// Pi's SQLite opener sets WAL and synchronous=NORMAL; FULL is set on the same
// connection before the storage opens, so every commit syncs the WAL.
const database = await openNodeSqliteDatabase(store);
await database.exec(`PRAGMA synchronous = ${synchronous.toUpperCase()}`);
const pragma = (await database.get('PRAGMA synchronous')).synchronous;
const journal = (await database.get('PRAGMA journal_mode')).journal_mode;
if (pragma !== SYNCHRONOUS[synchronous] || journal !== 'wal') throw new Error('durability setting not applied');
const harness = await Harness.open(await SqliteStorage.open(database), {
  models, registry,
  settings: { stream: { transport: 'sse', maxRetries: 0, cacheRetention: 'none' },
              retry: { enabled: false, maxRetries: 0 }, compaction: { enabled: false } },
  env: ({ cwd }) => new NodeExecutionEnv({ cwd }),
}, context);
harness.resume();

const watched = new Map();
async function conversation(bot) {
  const found = await harness.snapshot(Bot, bot, context);
  const handle = found && await harness.conversation(found.conversation, context);
  if (!handle) throw Object.assign(new Error('bot_not_found'), { code: 'bot_not_found' });
  if (!watched.has(bot)) {
    // Deliver the engine's own event stream to the controller, as the Rust
    // daemon writes its events to stdout.
    const stream = await watchEvents(harness, handle.id, context);
    stream.start(async events => { await emit({ event: 'pi', bot, events }); });
    watched.set(bot, stream);
  }
  return handle;
}

async function entries(handle) {
  const all = [];
  let cursor;
  do {
    const page = await handle.entries({}, 256, cursor, context);
    all.push(...page.items);
    cursor = page.next;
  } while (cursor);
  return all.reverse();
}

async function create(bot, workspace, parent) {
  if (await harness.snapshot(Bot, bot, context)) throw Object.assign(new Error('bot_exists'), { code: 'bot_exists' });
  const options = { ownership: { kind: 'ownerless' }, agent: { model: { provider: 'bench', modelId: model.id },
    instructions: 'Test agent.', cwd: workspace },
    init: async (tx, id) => { await tx.doc(Bot, bot, id); } };
  return parent ? parent.handle.fork(parent.at, options, context) : harness.createConversation(options, context);
}

// Submissions in flight, by bot and request ID.
const submitting = new Map();

async function submit(bot, requestId, prompt) {
  const handle = await conversation(bot);
  const existing = await handle.commit(tx => tx.submissionByRequest(handle.id, requestId), context);
  const submission = await handle.submit({ type: 'input', content: prompt, requestId }, context);
  if (!existing) {
    submission.wait(context).then(settled => emit({ event: 'turn_finished', turn: submission.id, data: {
      status: settled.status === 'done' ? 'completed' : 'failed', checkpoint: settled.answer ?? null } }));
  }
  return { turn: submission.id, duplicate: existing?.id === submission.id };
}

const operations = {
  async create({ bot, workspace }) {
    return { conversation: (await create(bot, workspace)).id };
  },
  async submit({ bot, request_id, prompt }) {
    // Requests run concurrently: a repeat of one still being submitted is
    // its duplicate, and waits for it rather than racing its lookup.
    const key = JSON.stringify([bot, request_id]);
    const first = submitting.get(key);
    if (first) return { turn: (await first).turn, duplicate: true };
    const started = submit(bot, request_id, prompt);
    submitting.set(key, started);
    try {
      return await started;
    } finally {
      submitting.delete(key);
    }
  },
  async resume({ bot }) {
    const handle = await conversation(bot);
    const newest = (await handle.entries({}, 1, undefined, context)).items[0];
    const done = newest?.kind === 'pi.assistant' && newest.model?.[0]?.stopReason === 'stop';
    return { status: done ? 'completed' : 'not_completed' };
  },
  async transcript({ bot }) {
    return { entries: await entries(await conversation(bot)) };
  },
  async entry({ bot, entry }) {
    const handle = await conversation(bot);
    const found = (await handle.entries({ minEntryId: entry, maxEntryId: entry }, 1, undefined, context)).items[0];
    if (!found) throw Object.assign(new Error('entry_not_found'), { code: 'entry_not_found' });
    return { entry: found };
  },
  async fork({ source, checkpoint, bot, workspace }) {
    const fork = await create(bot, workspace, { handle: await conversation(source), at: checkpoint });
    const head = (await fork.entries({}, 1, undefined, context)).items[0]?.id;
    return { conversation: fork.id, head };
  },
};

await emit({ event: 'ready', synchronous, journal_mode: journal });
for await (const line of createInterface({ input: process.stdin })) {
  const { id, op, ...params } = JSON.parse(line);
  // Requests run concurrently, like independent controller requests.
  (async () => {
    try {
      if (!operations[op]) throw Object.assign(new Error('unknown_op'), { code: 'unknown_op' });
      await emit({ id, result: await operations[op](params) });
    } catch (error) {
      // Only static codes; no prompts, provider bodies, or environment values.
      await emit({ id, error: { code: error?.code ?? 'adapter_error' } });
    }
  })();
}
for (const stream of watched.values()) await stream.stop();
await harness.close(context);
