"""What a helper's tool list costs: making agents and starting their turns,
with a few bots and with thousands, on this build or another binary.

A lead with Sonny's shape of declared tools (one with a fixed result, one
its client answers, a script tool that takes steps) makes agents three
ways: `create` with it as creator, as `agent run --new` does; `spawn_agent`
naming no tools; and `spawn_agent` naming an exact list. Then a turn of
each kind of agent it made. Each measure is the median and p95 of ROUNDS.
Synthetic model, no spend. Needs a release build.

    AGENT_TEST_RUNTIME=1 AGENTS=5,10000 .local/venv/bin/python -m bench.spawn_tools
    AGENT_BINARY=/path/to/main/agent ...   # the same against another build
"""
import json
import os
import statistics
import time
import unittest
from pathlib import Path

from bench.agent_tools_scale import Answering, SocketClient
from tests.test_runtime import ModelFixture

ROUNDS = int(os.environ.get('ROUNDS', '40'))
AGENTS = [int(n) for n in os.environ.get('AGENTS', '5,10000').split(',')]
TOOLS = 'echo,shell,read,wait,history,send_message,list_agents,spawn_agent,close_agent'
DECLARE = [{'name': 'ask', 'description': 'Ask.', 'parameters': {'type': 'object'}},
           {'name': 'memory', 'description': 'Notes.', 'parameters': {'type': 'object'}, 'answered_by': 'sonny'},
           {'name': 'exec', 'description': 'Run a script.', 'parameters': {'type': 'object'},
            'answered_by': 'sonny', 'steps': True}]


def summary(times):
    times = sorted(times)
    return statistics.median(times), times[max(0, int(len(times) * .95) - 1)]


class Bench(ModelFixture):
    handler = Answering

    def setUp(self):
        super().setUp()
        if os.environ.get('AGENT_BINARY'):
            self.binary = Path(os.environ['AGENT_BINARY'])

    def turn(self, client, bot, prompt, request_id):
        started = time.perf_counter()
        turn = client.request('submit', bot=bot, request_id=request_id, prompt=prompt)['result']['message']
        done = client.finished(turn, timeout=60)
        assert done['data']['status'] == 'completed', done
        return (time.perf_counter() - started) * 1000

    def measure(self, agents):
        client = SocketClient(self, TOOLS)
        self.addCleanup(client.close)
        lead = client.request('create', bot='p.lead', workspace=str(self.path), declare=DECLARE,
                              settings={'namespace': 'p'})['result']
        for i in range(agents):
            assert 'result' in client.request('create', bot=f'p.b{i}', workspace=str(self.path))
        # Every bot's turns, the ones the lead makes among them.
        client.request('follow', bot='*')
        times = {name: [] for name in ('create by the lead', 'spawn_agent, no tools', 'spawn_agent, tools named',
                                       'turn of one with its maker\'s tools', 'turn of one kept to a list')}
        for r in range(ROUNDS):
            started = time.perf_counter()
            made = client.request('create', bot=f'p.c{r}', workspace=str(self.path),
                                  created_by='p.lead', created_by_id=lead['bot_id'])
            times['create by the lead'].append((time.perf_counter() - started) * 1000)
            assert 'result' in made, made
            for name, called in (('spawn_agent, no tools', {'name': f'all{r}', 'message': 'hi'}),
                                 ('spawn_agent, tools named',
                                  {'name': f'kept{r}', 'message': 'hi', 'tools': ['echo', 'read', 'memory']})):
                prompt = 'calls:' + json.dumps([['spawn_agent', called]])
                times[name].append(self.turn(client, 'p.lead', prompt, f"{called['name']}"))
                child = f"p.{called['name']}"
                # Its first turn ends before the next is measured.
                client.receive(lambda m: m.get('event') == 'turn_finished' and m.get('bot') == child, timeout=30)
            times["turn of one with its maker's tools"].append(self.turn(client, f'p.all{r}', 'again', 'again'))
            times['turn of one kept to a list'].append(self.turn(client, f'p.kept{r}', 'again', 'again'))
        kept = client.request('inspect', bot=f'p.kept{ROUNDS - 1}')['result']
        print(f"{agents} bots: kept helper tools {kept['tools']} allowed {kept.get('allowed')}", flush=True)
        for name, measured in times.items():
            median, p95 = summary(measured)
            print(f'{agents:>6} bots  {name:<36} median {median:7.2f} ms  p95 {p95:7.2f} ms', flush=True)


for count in AGENTS:
    setattr(Bench, f'test_{count:05d}', lambda self, count=count: self.measure(count))

if __name__ == '__main__':
    unittest.main()
