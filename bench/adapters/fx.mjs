import { createFxAgent } from 'libfx';
import { baseUrl, config, emit, instructions, Output, prompt } from './common.mjs';

// Preserve FX's request serialization, SSE parser, history, and agent loop.
// Only route its host-owned HTTP transport to the synthetic provider.
export async function fixtureFetch(input, init = {}) {
  const url = String(input);
  const method = init.method ?? 'GET';
  if (method === 'GET' && url === 'https://ai-gateway.vercel.sh/coding-agent/v1/models') {
    return fetch(`${baseUrl}/models`, init);
  }
  if (method === 'POST' && url === `${baseUrl}/gateway`) return fetch(input, init);
  throw new Error('unexpected FX benchmark network request');
}

const bots = [];
try {
  // Retain every successfully created runtime for cleanup on partial failure.
  for (let index = 0; index < config.concurrency; index++) {
    bots.push(await createFxAgent({
      backend: 'native', apiKey: 'synthetic-not-a-secret', model: 'bench-model',
      instructions, tools: [], fetch: fixtureFetch, gatewayChatUrl: `${baseUrl}/gateway`,
      home: process.env.HOME, workspaceRoot: process.env.AGENT_BENCH_WORKSPACE,
    }));
  }
  await emit({ event: 'ready' });
  await Promise.all(bots.map(async (bot, agent) => {
    for (let index = 0; index < config.turns; index++) {
      const output = new Output(agent, index);
      await output.start();
      const turn = bot.prompt(prompt(agent, index));
      for await (const event of turn) {
        if (event.type === 'text_delta') await output.delta(event.delta);
      }
      if ((await turn.result).stopReason !== 'end_turn') throw new Error('FX turn failed');
      await output.end();
    }
  }));
} catch {
  console.error('FX benchmark adapter failed');
  process.exitCode = 1;
} finally {
  const closed = await Promise.allSettled(bots.map(bot => bot.close()));
  if (closed.some(result => result.status === 'rejected')) process.exitCode = 1;
}
