"""The agent tools against the `agent` commands they replace, at a few bots
and at thousands: a lead's turn that makes one call, by latency, and what
list_agents answers, by size, at the first, a middle and the last page.
Each call is made by a bot of its own, over the daemon's socket, so the
commands reach the daemon as an agent's do. Synthetic model, no spend.
Needs a release build and bubblewrap.

    AGENT_TEST_SANDBOX=1 AGENT_TEST_RUNTIME=1 AGENTS=5,700,5000 \\
        .local/venv/bin/python -m bench.agent_tools_scale
"""
import json
import os
import queue
import socket
import statistics
import subprocess
import threading
import time
import types
import unittest

from tests.test_runtime import Model, ModelFixture, PROTECTING
from bench.runtime_client import Client, clean_env, node_item, serve_args

# COMMANDS=1 runs only the commands, on a daemon without the tools (main
# before them), for the same cases against it.
COMMANDS = os.environ.get('COMMANDS') == '1'
TOOLS = 'echo,shell,wait' if COMMANDS else 'echo,shell,wait,send_message,list_agents,pause_agents'
ROUNDS = int(os.environ.get('ROUNDS', '15'))
AGENTS = [int(n) for n in os.environ.get('AGENTS', '5,5000').split(',')]


def cases(agents):
    # A page starts after a name, and names sort as text (p.b0, p.b1,
    # p.b10, ...): the middle page starts halfway through that order, the
    # last 60 names before the end.
    names = sorted(['p.lead', *(f'p.b{i}' for i in range(agents))])
    middle, last = names[len(names) // 2], names[max(len(names) - 61, 0)]
    every = {
        'send_message': ['send_message', {'to': 'p.b1', 'message': 'hi', 'delivery': 'queue'}],
        'agent run (shell)': ['shell', {'command': '"$AGENT_BIN" run --bot p.b1 --detach --delivery queue -- hi'}],
        'list_agents, first 50': ['list_agents', {'name': 'p.*'}],
        'list_agents, 256 from the middle': ['list_agents', {'name': 'p.*', 'limit': 256, 'after': middle}],
        'list_agents, last page': ['list_agents', {'name': 'p.*', 'limit': 256, 'after': last}],
        'list_agents *.lead': ['list_agents', {'name': '*.lead'}],
        'list_agents running': ['list_agents', {'name': 'p.*', 'running': True}],
        'list_agents approvals': ['list_agents', {'name': 'p.*', 'approvals': True}],
        'agent ls, first 50 (shell)': ['shell', {'command': '"$AGENT_BIN" ls --name "p.*" --limit 50'}],
        'agent ls *.lead (shell)': ['shell', {'command': '"$AGENT_BIN" ls --name "*.lead"'}],
        'agent ls --active (shell)': ['shell', {'command': '"$AGENT_BIN" ls --name "p.*" --active'}],
        'agent approvals (shell)': ['shell', {'command': '"$AGENT_BIN" approvals --bot "p.*"'}],
    }
    return {name: called for name, called in every.items() if not COMMANDS or called[0] == 'shell'}


class SocketClient(Client):
    """A client of the daemon's socket, as an agent's `agent` command is, in
    the stdio client's shape. The daemon listens there only when it serves
    no session on its stdin."""

    def __init__(self, fixture, tools):
        store = fixture.path / 'state.sqlite'
        sock = fixture.path / 'state.sqlite.sock'
        self.daemon = subprocess.Popen(
            [str(fixture.binary), *serve_args(store, fixture.url, extra=(*PROTECTING, '--socket', str(sock)))],
            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            env=clean_env(), start_new_session=True)
        for _ in range(200):
            if sock.exists():
                break
            time.sleep(0.05)
        self.connection = socket.socket(socket.AF_UNIX)
        self.connection.connect(str(sock))
        self.process = types.SimpleNamespace(stdin=self.connection.makefile('w'),
                                             stdout=self.connection.makefile('r'), poll=lambda: None)
        self.queue, self.saved, self.next_id = queue.Queue(), [], 0
        self.model, self.instructions = 'openai/synthetic-model', 'Test agent.'
        self.tools, self.settings = tools.split(','), None
        self.reader = threading.Thread(target=self.pump, daemon=True)
        self.reader.start()
        self.ready = self.receive(lambda m: m.get('event') == 'ready')

    def pump(self):
        for line in self.process.stdout:
            self.queue.put(json.loads(line))
        self.queue.put(None)

    def close(self, kill=False):
        self.connection.close()
        self.daemon.kill()
        self.daemon.wait(timeout=5)


class Answering(Model):
    """The synthetic model, its answers sent without Nagle's delay: its
    headers and body go in two writes, and the second otherwise waits about
    40 ms on the daemon's delayed acknowledgment whenever a call follows
    the last one closely, which swamped the differences measured here."""
    disable_nagle_algorithm = True


class Bench(ModelFixture):
    handler = Answering

    def output(self, client, bot, call_id):
        """`bot`'s result of `call_id`."""
        node, after = None, 0
        while page := client.request('events', bot=bot, after=after, limit=256)['result']['events']:
            node = next((e['data']['node'] for e in reversed(page) if e['event'] == 'tool_completed'
                         and e['data']['call_id'] == call_id), node)
            after = page[-1]['cursor']
        return node_item(client, bot, node)['result']['output']

    def caller(self, client):
        """A new bot to make the call, so no call's history carries another's
        answers (a 30 KB page in the history slows every call after it)."""
        self.callers += 1
        bot = f'c.{self.callers}'
        assert 'result' in client.request('create', bot=bot, workspace=str(self.path))
        client.request('follow', bot=bot)
        return bot

    def submit(self, client, bot, called):
        return client.request('submit', bot=bot, request_id='go',
                              prompt='calls:' + json.dumps([called]))['result']['message']

    def measure(self, agents):
        self.callers = 0
        client = SocketClient(self, TOOLS)
        self.addCleanup(client.close)
        started = time.perf_counter()
        client.request('create', bot='p.lead', workspace=str(self.path))
        for i in range(agents):
            assert 'result' in client.request('create', bot=f'p.b{i}', workspace=str(self.path))
        made = time.perf_counter() - started
        # A socket session hears a bot's turns once it follows them.
        client.request('follow', bot='p.b1')
        print(f'{agents} bots made in {made:.1f} s', flush=True)
        rows = []
        # Round by round through every case, so a drift over the run falls
        # on all of them alike.
        listed = cases(agents)
        times = {name: [] for name in listed}
        for r in range(ROUNDS):
            for name, called in listed.items():
                bot = self.caller(client)
                started = time.perf_counter()
                done = client.finished(self.submit(client, bot, called), timeout=60)
                times[name].append((time.perf_counter() - started) * 1000)
                assert done['data']['status'] == 'completed', done
                # A message starts a turn of b1's: let it end, so the next
                # call does not share the model with it.
                if called[0] == 'send_message' or 'agent run' in name:
                    client.receive(lambda m: m.get('event') == 'turn_finished' and m.get('bot') == 'p.b1',
                                   timeout=30)
                if r == ROUNDS - 1:
                    out = self.output(client, bot, f'{called[0]}-1')
                    note = ''
                    if called[0] == 'list_agents':
                        answer = json.loads(out)
                        key = 'approvals' if 'approvals' in answer else 'bots'
                        note = f"{len(answer[key])} listed, next_after {answer['next_after']!r}"
                    elif called[0] == 'shell':
                        note = f"exit {json.loads(out).get('exit_code', 'none')}"
                    rows.append((name, statistics.median(times[name]), max(times[name]),
                                 len(out.encode()), note))
        # The cord: from the submit to the daemon's word that it is paused,
        # then from the resume to the turn's end.
        cords = [('pause_agents', ['pause_agents', {'reason': 'bench'}]),
                 ('agent pause (shell)', ['shell', {'command': '"$AGENT_BIN" pause --reason bench'}])]
        cords = cords[1:] if COMMANDS else cords
        paused = {name: [] for name, _ in cords}
        resumed = {name: [] for name, _ in cords}
        for r in range(ROUNDS):
            for name, called in cords:
                bot = self.caller(client)
                started = time.perf_counter()
                turn = self.submit(client, bot, called)
                client.receive(lambda m: m.get('event') == 'paused', timeout=30)
                paused[name].append((time.perf_counter() - started) * 1000)
                started = time.perf_counter()
                client.request('resume')
                client.finished(turn, timeout=60)
                resumed[name].append((time.perf_counter() - started) * 1000)
        for name, _ in cords:
            rows.append((name, statistics.median(paused[name]), max(paused[name]), None,
                         f'resume to end {statistics.median(resumed[name]):.1f} ms'))
        for name, median, worst, size, note in rows:
            size = '' if size is None else f'{size:>7} B'
            print(f'{agents:>5} bots  {name:<34} median {median:7.1f} ms  max {worst:7.1f} ms  {size}  {note}',
                  flush=True)


for count in AGENTS:
    setattr(Bench, f'test_{count:05d}', lambda self, count=count: self.measure(count))

if __name__ == '__main__':
    unittest.main()
