import { Agent } from '@earendil-works/pi-agent-core';
import { streamSimple } from '@earendil-works/pi-ai/api/openai-responses';
import { baseUrl, config, emit, instructions, Output, prompt } from './common.mjs';

const model = {
  id: 'bench-model', name: 'Synthetic benchmark', api: 'openai-responses',
  provider: 'openai', baseUrl, reasoning: false, input: ['text'],
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 1000000, maxTokens: 1000000,
};

try {
  const bots = Array.from({ length: config.concurrency }, () => new Agent({
    initialState: { model, systemPrompt: instructions, tools: [] },
    // Exercise Pi's real HTTP/SSE provider implementation and agent loop.
    streamFn: (m, context, options) => streamSimple(m, context, {
      ...options, apiKey: 'synthetic-not-a-secret', maxRetries: 0,
      transport: 'sse', cacheRetention: 'none',
    }),
  }));
  await emit({ event: 'ready' });
  await Promise.all(bots.map(async (bot, agent) => {
    for (let turn = 0; turn < config.turns; turn++) {
      const output = new Output(agent, turn);
      const unsubscribe = bot.subscribe(async event => {
        if (event.type === 'message_update' && event.assistantMessageEvent.type === 'text_delta') {
          await output.delta(event.assistantMessageEvent.delta);
        }
      });
      await output.start();
      try {
        await bot.prompt(prompt(agent, turn));
        if (bot.state.errorMessage || bot.state.messages.at(-1)?.stopReason !== 'stop') {
          throw new Error('Pi turn failed');
        }
        await output.end();
      } finally {
        unsubscribe();
      }
    }
  }));
} catch {
  // Errors contain no provider response, prompt, or environment values.
  console.error('Pi benchmark adapter failed');
  process.exitCode = 1;
}
