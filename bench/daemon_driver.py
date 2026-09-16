"""Drive `agent serve` over its own stdio JSONL protocol for the streaming workload.

The observer runs this, not the target: the daemon is the only process
charged. Daemon responses and events are translated into the observer's
fixed-size chunk protocol, so the Rust engine is measured through the surface
software would use rather than a benchmark-only entry point.
"""
import json

ALLOWED_DIAGNOSTICS = {'provider_connection_timeout', 'provider_connection_failed', 'provider_stream_failed',
                       'missing_completion', 'output_closed'}


def prompt(config, agent, turn):
    return f'BENCH agent={agent} turn={turn}\n' + 'x' * config['history_bytes']


class DaemonDriver:
    def __init__(self, config, workspace):
        self.config = config
        self.workspace = str(workspace)
        self.next_id = 0
        self.pending = {}   # request id -> (op, agent)
        self.turn_of = {}   # agent -> current turn index
        self.progress = {}  # agent -> [bytes, seq]
        self.created = 0
        self.finished = 0
        self.failed = False
        self.done = False
        self.outgoing = []

    def request(self, op, agent=None, **params):
        self.next_id += 1
        self.pending[self.next_id] = (op, agent)
        self.outgoing.append(json.dumps({'id': self.next_id, 'op': op, **params}, separators=(',', ':')) + '\n')

    def drain(self):
        lines, self.outgoing = self.outgoing, []
        return lines

    def submit(self, agent):
        turn = self.turn_of[agent]
        self.progress[agent] = [0, 0]
        self.request('submit', agent, bot=f'b{agent}', request_id=f'{agent}-{turn}',
                     prompt=prompt(self.config, agent, turn))
        return {'event': 'turn_start', 'agent': str(agent), 'turn': str(turn)}

    def handle(self, message):
        """Observer events for one daemon message; queues any follow-up requests."""
        if not isinstance(message, dict):
            raise ValueError('daemon message must be an object')
        if 'id' in message:
            return self.response(message)
        kind = message.get('event')
        if kind == 'ready':
            for agent in range(self.config['concurrency']):
                self.request('create', agent, bot=f'b{agent}', workspace=self.workspace)
            return []
        bot = message.get('bot')
        if not isinstance(bot, str) or not bot.startswith('b') or not bot[1:].isdigit():
            return []
        agent = int(bot[1:])
        if kind == 'text_delta':
            return self.delta(agent, message.get('text'))
        if kind == 'turn_finished':
            return self.finish(agent, message.get('data') or {})
        return []

    def response(self, message):
        op, agent = self.pending.pop(message['id'])
        if 'error' in message:
            raise ValueError('daemon request failed')
        if op == 'create':
            self.created += 1
            if self.created == self.config['concurrency']:
                events = [{'event': 'ready'}]
                for index in range(self.config['concurrency']):
                    self.turn_of[index] = 0
                    events.append(self.submit(index))
                return events
        return []

    def delta(self, agent, text):
        if agent not in self.progress:
            raise ValueError('delta for an inactive turn')
        if not isinstance(text, str) or text.strip('x'):
            raise ValueError('unexpected output bytes')
        state = self.progress[agent]
        state[0] += len(text)
        limit = self.config['chunks'] * self.config['chunk_bytes']
        if state[0] > limit:
            raise ValueError('excess output')
        events = []
        turn = str(self.turn_of[agent])
        while state[1] < state[0] // self.config['chunk_bytes']:
            events.append({'event': 'chunk', 'agent': str(agent), 'turn': turn,
                           'seq': state[1], 'bytes': self.config['chunk_bytes']})
            state[1] += 1
        return events

    def finish(self, agent, data):
        state = self.progress.pop(agent, None)
        if state is None:
            raise ValueError('finish for an inactive turn')
        if data.get('status') != 'completed':
            code = data.get('error')
            self.failed = True
            self.done = True
            self.request('shutdown')
            return [{'event': 'diagnostic', 'stage': 'benchmark',
                     'code': code if code in ALLOWED_DIAGNOSTICS else 'benchmark_failed'}]
        if state[0] != self.config['chunks'] * self.config['chunk_bytes']:
            raise ValueError('incomplete output')
        events = [{'event': 'turn_end', 'agent': str(agent), 'turn': str(self.turn_of[agent])}]
        self.turn_of[agent] += 1
        if self.turn_of[agent] < self.config['turns']:
            events.append(self.submit(agent))
        else:
            self.finished += 1
            if self.finished == self.config['concurrency']:
                self.done = True
                self.request('shutdown')
        return events
