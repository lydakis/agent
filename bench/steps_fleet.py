"""Daemon steps at fleet size, through the daemon's socket alone (no
Sonny): bots whose model calls a script tool (`exec`, declared with
`steps`), each call then running `STEPS` `echo` steps one after another
with `run_step`, every bot's at once, before its client answers the call.
Reports each step's time from `run_step` sent to its reply (p50, p99, max),
the steps the daemon refused, its store jobs per step, the bytes and line it
held at its peak, and its memory; and beside the fleet, `SIDE` other bots
running plain turns, whose turn times are compared with the same turns on
an idle daemon. Synthetic model, no spend; needs a release build.

    AGENT_TEST_RUNTIME=1 AGENTS=1000,10000 STEPS=5 [BINARY=PATH] \\
        python3 -m bench.steps_fleet
"""
import json
import multiprocessing
import os
import socket
import statistics
import threading
import time
import unittest
from pathlib import Path

from bench.socket_client import SocketClient
from tests.test_runtime import ModelFixture, PROTECTING

AGENTS = [int(n) for n in os.environ.get('AGENTS', '1000').split(',')]
STEPS = int(os.environ.get('STEPS', '5'))
SIDE = int(os.environ.get('SIDE', '32'))
SIDE_TURNS = int(os.environ.get('SIDE_TURNS', '5'))
EXEC = {'name': 'exec', 'description': 'Run a script.',
        'parameters': {'type': 'object', 'properties': {'code': {'type': 'string'}},
                       'required': ['code'], 'additionalProperties': False},
        'answered_by': 'bench', 'steps': True}


class Pipe:
    """One socket session that sends without waiting: replies are kept by
    id with the time they came, and events by turn."""

    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.connect(str(path))
        self.reader = self.socket.makefile('r')
        self.lock = threading.Condition()
        self.replies, self.events, self.next_id = {}, {}, 0
        self.worker = threading.Thread(target=self.read, daemon=True)
        self.worker.start()

    def read(self):
        try:
            for line in self.reader:
                now = time.perf_counter()
                message = json.loads(line)
                with self.lock:
                    if 'id' in message:
                        self.replies[message['id']] = (now, message)
                    elif 'turn' in message and 'event' in message:
                        self.events.setdefault(message['turn'], []).append((now, message))
                    self.lock.notify_all()
        except (OSError, ValueError):
            pass

    def send(self, op, **params):
        self.next_id += 1
        self.socket.sendall((json.dumps(dict(id=self.next_id, op=op, **params)) + '\n').encode())
        return self.next_id, time.perf_counter()

    def send_many(self, requests):
        """Send a batch in one write; their ids and the time they left."""
        lines, ids = [], []
        for op, params in requests:
            self.next_id += 1
            ids.append(self.next_id)
            lines.append(json.dumps(dict(id=self.next_id, op=op, **params)))
        sent = time.perf_counter()
        self.socket.sendall(('\n'.join(lines) + '\n').encode())
        return ids, sent

    def wait(self, test, timeout):
        deadline = time.monotonic() + timeout
        with self.lock:
            while not test():
                left = deadline - time.monotonic()
                if left <= 0:
                    raise TimeoutError
                self.lock.wait(left)

    def request(self, op, timeout=60, **params):
        request, _ = self.send(op, **params)
        self.wait(lambda: request in self.replies, timeout)
        with self.lock:
            return self.replies.pop(request)[1]

    def take(self, request):
        with self.lock:
            return self.replies.pop(request, None)

    def close(self):
        try:
            self.socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.worker.join(timeout=2)
        self.reader.close()
        self.socket.close()


def side_turns(pipe, stop, name):
    """Plain turns on other bots, one after another on each, until
    `stop`: each turn's time from submit to its end."""
    times = []
    for round_ in range(SIDE_TURNS):
        if stop is not None and stop.is_set() and round_ > 0:
            break
        ids, sent = pipe.send_many([('submit', dict(bot=f's.b{i}', request_id=f'{name}{round_}-{i}',
                                                    prompt='hello', delivery='reject'))
                                    for i in range(SIDE)])
        turns = []
        for request in ids:
            pipe.wait(lambda: request in pipe.replies, 60)
            turns.append((pipe.take(request)[1])['result']['message'])

        def ended(turn):
            return any(e['event'] == 'turn_finished' for _, e in pipe.events.get(turn, []))
        pipe.wait(lambda: all(ended(t) for t in turns), 120)
        with pipe.lock:
            for turn in turns:
                at = next(t for t, e in pipe.events[turn] if e['event'] == 'turn_finished')
                times.append((at - sent) * 1000)
                pipe.events.pop(turn, None)
    return times


def side_process(path, go, stop, out):
    """The other bots' turns, timed from a process of their own, so the
    fleet's client, parsing thousands of replies a second, does not time
    them: once on the idle daemon, then from `go` until `stop`."""
    pipe = Pipe(path)
    try:
        bots = [f's.b{i}' for i in range(SIDE)]
        ids, _ = pipe.send_many([('follow', dict(bot=bot, kinds=['turn_finished'], after=0)) for bot in bots])
        pipe.wait(lambda: all(i in pipe.replies for i in ids), 120)
        out.put(('idle', side_turns(pipe, None, 'idle')))
        go.wait()
        out.put(('busy', side_turns(pipe, stop, 'busy')))
    finally:
        pipe.close()


def percentile(values, p):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, round(p / 100 * (len(ordered) - 1)))]


def rss_kib(pid):
    with open(f'/proc/{pid}/status') as status:
        for line in status:
            if line.startswith('VmRSS:'):
                return int(line.split()[1])
    return None


def threads_cpu_s(pid):
    """Each of the daemon's threads' processor time, by name."""
    tick = os.sysconf('SC_CLK_TCK')
    out = {}
    for task in Path(f'/proc/{pid}/task').iterdir():
        try:
            stat = (task / 'stat').read_text()
        except OSError:
            continue
        name = stat[stat.index('(') + 1:stat.rindex(')')]
        fields = stat[stat.rindex(')') + 2:].split()
        out[name] = out.get(name, 0) + (int(fields[11]) + int(fields[12])) / tick
    return out


def store_ops(stats):
    """The store's jobs by kind, from `stats`."""
    store = stats['store']
    ops = {k: v['count'] for k, v in store['operations'].items()}
    ops['commit groups'] = store['groups']['count']
    for label in ('step_open', 'step_close', 'commit'):
        if label in store['operations']:
            ops[f'{label} ran_ms'] = store['operations'][label]['ran_ms']
            ops[f'{label} queued_ms'] = store['operations'][label]['queued_ms']
    return ops


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class StepsFleet(ModelFixture):
    def test_fleet(self):
        results = [self.run_fleet(agents) for agents in AGENTS]
        for result in results:
            print(json.dumps(result), flush=True)
        (Path(__file__).resolve().parent.parent / '.local/steps_fleet.json').write_text(json.dumps(results, indent=1))

    def run_fleet(self, agents):
        store = self.path / f'steps-{agents}.sqlite'
        binary = os.environ.get('BINARY', self.binary)
        self.model.call_script = [('exec', {'code': 'run()'})]
        # One process slot: the footprint's steps wait in line behind one
        # command. Echo steps and plain turns take none.
        daemon = SocketClient(binary, store, self.url, 'echo,shell',
                              extra=(*PROTECTING, '--max-active', '0', '--max-processes', '1'))
        pipe = Pipe(daemon.socket_path)
        forked = multiprocessing.get_context('fork')
        go, stop, out = forked.Event(), forked.Event(), forked.Queue()
        sider = None
        try:
            pid = daemon.process.pid
            create = [('create', dict(bot=f'p.b{i}', workspace=str(self.path), declare=[EXEC],
                                      model='openai/synthetic-model', instructions='Test agent.',
                                      tools=['echo', 'shell'])) for i in range(agents)]
            create += [('create', dict(bot=f's.b{i}', workspace=str(self.path), model='openai/synthetic-model',
                                       instructions='Test agent.', tools=['echo'], declare=[EXEC]))
                       for i in range(SIDE)]
            for start in range(0, len(create), 500):
                ids, _ = pipe.send_many(create[start:start + 500])
                pipe.wait(lambda: all(i in pipe.replies for i in ids), 120)
                for i in ids:
                    made = pipe.take(i)[1]
                    assert 'result' in made, made
            # The session follows its bots for their calls and ends only.
            kinds = ['tool_started', 'turn_finished']
            bots = [f'p.b{i}' for i in range(agents)] + ['s.b0']
            for start in range(0, len(bots), 500):
                ids, _ = pipe.send_many([('follow', dict(bot=bot, kinds=kinds, after=0))
                                         for bot in bots[start:start + 500]])
                pipe.wait(lambda: all(i in pipe.replies for i in ids), 120)
                for i in ids:
                    followed = pipe.take(i)[1]
                    assert 'result' in followed, followed
            sider = forked.Process(target=side_process, args=(str(daemon.socket_path), go, stop, out))
            sider.start()
            kind, idle = out.get(timeout=600)
            assert kind == 'idle'
            # The other bots' turns from here on run in their own process,
            # never at once with the one step alone.
            # One step alone, on one bot whose call waits: what a step costs
            # with nothing else asking.
            pipe.send('submit', bot='s.b0', request_id='alone', prompt='script', delivery='reject')
            alone_turn = None
            pipe.wait(lambda: any(e['event'] == 'tool_started' for t, es in pipe.events.items()
                                  for _, e in es), 60)
            with pipe.lock:
                alone_turn = next(t for t, es in pipe.events.items() if any(e['event'] == 'tool_started' for _, e in es))
            alone = []
            for n in range(200):
                request, sent = pipe.send('run_step', bot='s.b0', turn=alone_turn, call_id='script-0',
                                          step=str(n), name='echo', arguments={'text': 'hi'})
                pipe.wait(lambda: request in pipe.replies, 30)
                at, reply = pipe.take(request)
                assert 'result' in reply, reply
                alone.append((at - sent) * 1000)
            pipe.request('tool_result', bot='s.b0', turn=alone_turn, call_id='script-0', output='done')
            pipe.wait(lambda: any(e['event'] == 'turn_finished' for _, e in pipe.events.get(alone_turn, [])), 60)
            with pipe.lock:
                pipe.events.pop(alone_turn, None)
            # Every fleet bot's turn calls `exec` and waits for its answer.
            submitted = time.perf_counter()
            turns = {}
            for start in range(0, agents, 1000):
                ids, _ = pipe.send_many([('submit', dict(bot=f'p.b{i}', request_id='t', prompt='script',
                                                         delivery='reject'))
                                         for i in range(start, min(agents, start + 1000))])
                pipe.wait(lambda: all(i in pipe.replies for i in ids), 300)
                for n, i in enumerate(ids):
                    answer = pipe.take(i)[1]
                    assert 'result' in answer, answer
                    turns[start + n] = answer['result']['message']

            def calling(turn):
                return any(e['event'] == 'tool_started' for _, e in pipe.events.get(turn, []))
            pipe.wait(lambda: all(calling(t) for t in turns.values()), 600)
            calls_s = time.perf_counter() - submitted
            footprint = self.footprint(pipe, pid, turns, agents)
            before = pipe.request('stats')['result']
            ops_before = store_ops(before)
            # Side turns run while the fleet steps.
            go.set()
            # Every bot's first step at once, then each bot's next as its
            # last one answers.
            waits, refused, done = [], {}, 0
            sent_at, next_step, peak = {}, {}, {'asked': 0, 'in_line': 0, 'in_flight': 0, 'bytes': 0, 'rss_kib': 0}
            started = time.perf_counter()
            cpu_before, client_before = threads_cpu_s(pid), time.process_time()

            def ask(bots):
                requests = [('run_step', dict(bot=f'p.b{b}', turn=turns[b], call_id='script-0',
                                              step=str(next_step.get(b, 1)), name='echo',
                                              arguments={'text': f'step {next_step.get(b, 1)}'}))
                            for b in bots]
                ids, sent = pipe.send_many(requests)
                for request, b in zip(ids, bots):
                    sent_at[request] = (b, sent)

            for start in range(0, agents, 2000):
                ask(list(range(start, min(agents, start + 2000))))
            last_sample = 0
            while done < agents:
                with pipe.lock:
                    while not any(r in pipe.replies for r in sent_at):
                        pipe.lock.wait(30)
                    ready = [(r, pipe.replies.pop(r)) for r in list(sent_at) if r in pipe.replies]
                again = []
                for request, (at, reply) in ready:
                    b, sent = sent_at.pop(request)
                    if 'error' in reply:
                        code = reply['error']['code'] if isinstance(reply['error'], dict) else reply['error']
                        refused[code] = refused.get(code, 0) + 1
                        again.append(b)
                        continue
                    assert reply['result']['output'] == f'step {next_step.get(b, 1)}', reply
                    waits.append((at - sent) * 1000)
                    next_step[b] = next_step.get(b, 1) + 1
                    if next_step[b] > STEPS:
                        done += 1
                    else:
                        again.append(b)
                if again:
                    ask(again)
                if time.perf_counter() - last_sample > .2:
                    last_sample = time.perf_counter()
                    stats = pipe.request('stats')['result']['steps']
                    for key in ('asked', 'in_line', 'in_flight', 'bytes'):
                        peak[key] = max(peak[key], stats[key])
                    peak['rss_kib'] = max(peak['rss_kib'], rss_kib(pid) or 0)
            steps_s = time.perf_counter() - started
            cpu = {name: round((spent - cpu_before.get(name, 0)) / steps_s, 2)
                   for name, spent in threads_cpu_s(pid).items() if spent - cpu_before.get(name, 0) > .01}
            cpu['bench client'] = round((time.process_time() - client_before) / steps_s, 2)
            stop.set()
            after = pipe.request('stats')['result']
            ops = {k: v - ops_before.get(k, 0) for k, v in store_ops(after).items() if v - ops_before.get(k, 0)}
            # Every call answered, every turn ended.
            for start in range(0, agents, 2000):
                ids, _ = pipe.send_many([('tool_result', dict(bot=f'p.b{b}', turn=turns[b], call_id='script-0',
                                                              output='done'))
                                         for b in range(start, min(agents, start + 2000))])
                pipe.wait(lambda: all(i in pipe.replies for i in ids), 300)
                for i in ids:
                    answer = pipe.take(i)[1]
                    assert 'result' in answer, answer

            def finished(turn):
                return any(e['event'] == 'turn_finished' for _, e in pipe.events.get(turn, []))
            pipe.wait(lambda: all(finished(t) for t in turns.values()), 600)
            kind, side_times = out.get(timeout=300)
            assert kind == 'busy'
            sider.join(timeout=60)
            asked = agents * STEPS
            return {
                'agents': agents, 'steps_each': STEPS, 'steps': len(waits),
                'alone_ms_p50': round(percentile(alone, 50), 3), 'alone_ms_p99': round(percentile(alone, 99), 3),
                'calls_ready_s': round(calls_s, 2), 'steps_s': round(steps_s, 2),
                'steps_per_s': round(len(waits) / steps_s),
                'step_ms_p50': round(percentile(waits, 50), 3), 'step_ms_p99': round(percentile(waits, 99), 3),
                'step_ms_max': round(max(waits), 3),
                'refused': refused, 'daemon_refused': after['steps']['refused'] - before['steps']['refused'],
                'store_ops': ops, 'cpu_share_by_thread': cpu,
                'store_ops_per_step': round(sum(v for k, v in ops.items() if k in ('step_open', 'step_close')) / asked, 3),
                'peak': peak, 'after': {k: after['steps'][k] for k in ('in_flight', 'in_line', 'commands', 'bytes')},
                'side_idle_ms_p50': round(percentile(idle, 50), 2), 'side_idle_ms_p99': round(percentile(idle, 99), 2),
                'side_busy_ms_p50': round(percentile(side_times, 50), 2) if side_times else None,
                'side_busy_ms_p99': round(percentile(side_times, 99), 2) if side_times else None,
                'side_busy_turns': len(side_times),
                'daemon_rss_kib': rss_kib(pid),
                'footprint': footprint,
            }
        finally:
            stop.set()
            go.set()
            if sider is not None:
                sider.join(timeout=60)
                if sider.is_alive():
                    sider.kill()
            pipe.close()
            daemon.close()

    def footprint(self, pipe, pid, turns, agents):
        """What a step waiting in line holds, measured: the daemon's RSS
        with every call waiting, then with one step of every call but one
        waiting in line behind that one's command, which holds the only
        process slot. Before any other step ran, so no freed memory the
        allocator kept counts. Then the command ends and the line drains."""
        def settled():
            time.sleep(1)
            return rss_kib(pid)
        before = settled()
        holder, sent = pipe.send('run_step', bot='p.b0', turn=turns[0], call_id='script-0', step='f',
                                 name='shell', arguments={'command': 'sleep 3'})
        while pipe.request('stats')['result']['steps']['commands'] == 0:
            time.sleep(.01)
        lined = []
        for start in range(1, agents, 2000):
            ids, _ = pipe.send_many([('run_step', dict(bot=f'p.b{b}', turn=turns[b], call_id='script-0', step='f',
                                                       name='shell', arguments={'command': 'true'}))
                                     for b in range(start, min(agents, start + 2000))])
            lined += ids
        while pipe.request('stats')['result']['steps']['in_line'] < agents - 1:
            time.sleep(.05)
        steps = pipe.request('stats')['result']['steps']
        waiting = settled()
        pipe.wait(lambda: holder in pipe.replies and all(i in pipe.replies for i in lined), 600)
        for i in [holder, *lined]:
            reply = pipe.take(i)[1]
            assert 'result' in reply, reply
        return {'rss_kib_calls_waiting': before, 'rss_kib_steps_in_line': waiting,
                'steps_in_line': steps['in_line'], 'accounted_bytes': steps['bytes'],
                'measured_bytes_per_step': round((waiting - before) * 1024 / max(1, steps['in_line']))}


if __name__ == '__main__':
    unittest.main(argv=['steps_fleet', 'StepsFleet'])
