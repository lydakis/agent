import { once } from 'node:events';

export const config = JSON.parse(process.env.AGENT_BENCH_WORKLOAD);
export const port = Number(process.env.AGENT_BENCH_PORT);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid fixture port');
export const baseUrl = `http://127.0.0.1:${port}/v1`;
export const instructions = 'Complete the synthetic benchmark turn.';
export const prompt = (agent, turn) => `BENCH agent=${agent} turn=${turn}\n${'x'.repeat(config.history_bytes)}`;

export async function emit(event) {
  if (!process.stdout.write(`${JSON.stringify(event)}\n`)) await once(process.stdout, 'drain');
}

// Engine deltas need not preserve SSE frame boundaries. Validate bytes, then
// normalize only for the observer's fixed-size logical chunk protocol.
export class Output {
  constructor(agent, turn) {
    this.agent = String(agent);
    this.turn = String(turn);
    this.bytes = 0;
    this.seq = 0;
  }
  async start() {
    await emit({ event: 'turn_start', agent: this.agent, turn: this.turn });
  }
  async delta(text) {
    if (typeof text !== 'string' || /[^x]/.test(text)) throw new Error('unexpected output bytes');
    this.bytes += text.length;
    if (this.bytes > config.chunks * config.chunk_bytes) throw new Error('excess output');
    while (this.seq < Math.floor(this.bytes / config.chunk_bytes)) {
      await emit({ event: 'chunk', agent: this.agent, turn: this.turn,
                   seq: this.seq++, bytes: config.chunk_bytes });
    }
  }
  async end() {
    if (this.bytes !== config.chunks * config.chunk_bytes) throw new Error('incomplete output');
    await emit({ event: 'turn_end', agent: this.agent, turn: this.turn });
  }
}
