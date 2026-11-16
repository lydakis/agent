"""The same fleet work two ways, through the daemon's socket alone (no
Sonny): every fleet bot's turn runs `CALLS` `echo` tools, either as
direct calls the model makes one round at a time (`MODE=direct`, which
any release since 0.9.5 runs) or as daemon steps of one script call the
bench answers (`MODE=steps`). Every fleet turn is submitted at once.

Reported for each: each turn's time from submit to its end (p50, p99,
max) and the fleet's wall time; the daemon's processor time, all its
threads, a turn, and the bench client's beside it; the daemon's peak
memory over the run and the peak above idle a fleet bot; the model
requests a turn; and `SIDE` other bots' plain turns, timed from their own
process, idle and while the fleet runs. Synthetic model, no spend; needs a
release build.

    AGENT_TEST_RUNTIME=1 MODE=direct AGENTS=400,2000 [BINARY=PATH] \\
        python3 -m bench.code_mode_vs_direct

`STEPS_ONLY=1` with `MODE=steps` sets the fleet bots' `steps_only` to
`["*"]`, as Sonny's code mode does: their model is shown the script tool
alone.
"""
import json
import multiprocessing
import os
import queue
import threading
import time
import unittest
from pathlib import Path

from bench.socket_client import SocketClient
from bench.steps_fleet import SIDE, Pipe, percentile, rss_kib, side_process, threads_cpu_s
from tests.test_runtime import ModelFixture, PROTECTING

MODE = os.environ.get('MODE', 'direct')
AGENTS = [int(n) for n in os.environ.get('AGENTS', '400').split(',')]
CALLS = int(os.environ.get('CALLS', '5'))
STEPS_ONLY = os.environ.get('STEPS_ONLY') == '1'
EXEC = {'name': 'exec', 'description': 'Run a script.',
        'parameters': {'type': 'object', 'properties': {'code': {'type': 'string'}},
                       'required': ['code'], 'additionalProperties': False},
        'answered_by': 'bench', 'steps': True}


class Feed(Pipe):
    """A pipe that also hands every reply and event to a queue, in the
    order they came."""

    def __init__(self, path):
        self.feed = queue.Queue()
        super().__init__(path)

    def read(self):
        try:
            for line in self.reader:
                now = time.perf_counter()
                message = json.loads(line)
                self.feed.put((now, message))
        except (OSError, ValueError):
            pass
        finally:
            self.feed.put(None)

    def request(self, op, timeout=60, **params):
        request, _ = self.send(op, **params)
        deadline = time.monotonic() + timeout
        while True:
            item = self.feed.get(timeout=max(0.01, deadline - time.monotonic()))
            if item is not None and item[1].get('id') == request:
                return item[1]


class Peak(threading.Thread):
    """The daemon's resident memory, read every 20 ms; the highest kept."""

    def __init__(self, pid):
        super().__init__(daemon=True)
        self.pid, self.peak, self.done = pid, 0, threading.Event()
        self.start()

    def run(self):
        while not self.done.is_set():
            try:
                self.peak = max(self.peak, rss_kib(self.pid) or 0)
            except OSError:
                return
            time.sleep(.02)

    def stop(self):
        self.done.set()
        self.join()
        return self.peak


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class CodeModeVsDirect(ModelFixture):
    def test_fleet(self):
        results = [self.run_fleet(agents) for agents in AGENTS]
        for result in results:
            print(json.dumps(result), flush=True)
        (Path(__file__).resolve().parent.parent / f'.local/code_mode_vs_direct-{MODE}.json').write_text(
            json.dumps(results, indent=1))

    def run_fleet(self, agents):
        store = self.path / f'{MODE}-{agents}.sqlite'
        binary = os.environ.get('BINARY', self.binary)
        steps = MODE == 'steps'
        self.model.call_script = [('exec', {'code': 'run()'})] if steps else [('echo', {'text': 'hi'})] * CALLS
        requests = {'n': 0, 'bytes': 0}
        handle = self.model.RequestHandlerClass.do_POST

        def counted(handler):
            requests['n'] += 1
            requests['bytes'] += int(handler.headers.get('Content-Length') or 0)
            return handle(handler)
        self.model.RequestHandlerClass.do_POST = counted
        self.addCleanup(setattr, self.model.RequestHandlerClass, 'do_POST', handle)
        daemon = SocketClient(binary, store, self.url, 'echo', extra=(*PROTECTING, '--max-active', '0'))
        pipe = Feed(daemon.socket_path)
        forked = multiprocessing.get_context('fork')
        go, stop, out = forked.Event(), forked.Event(), forked.Queue()
        sider = None
        try:
            pid = daemon.process.pid
            fleet = dict(workspace=str(self.path), model='openai/synthetic-model', instructions='Test agent.',
                         tools=['echo'], **({'declare': [EXEC]} if steps else {}),
                         **({'settings': {'steps_only': ['*']}} if steps and STEPS_ONLY else {}))
            create = [('create', dict(bot=f'p.b{i}', **fleet)) for i in range(agents)]
            create += [('create', dict(bot=f's.b{i}', workspace=str(self.path), model='openai/synthetic-model',
                                       instructions='Test agent.', tools=['echo'])) for i in range(SIDE)]
            kinds = ['tool_started', 'turn_finished'] if steps else ['turn_finished']
            follows = [('follow', dict(bot=f'p.b{i}', kinds=kinds, after=0)) for i in range(agents)]
            for batch in (create, follows):
                for start in range(0, len(batch), 500):
                    ids, _ = pipe.send_many(batch[start:start + 500])
                    self.replies(pipe, set(ids))
            # The other bots' plain turns, timed idle first; what one costs
            # the daemon then is taken off the fleet's processor time.
            side_cpu = sum(threads_cpu_s(pid).values())
            sider = forked.Process(target=side_process, args=(str(daemon.socket_path), go, stop, out))
            sider.start()
            kind, idle = out.get(timeout=600)
            assert kind == 'idle'
            side_cpu = (sum(threads_cpu_s(pid).values()) - side_cpu) / len(idle)
            time.sleep(1)
            idle_kib = rss_kib(pid)
            peak = Peak(pid)
            threads_before = threads_cpu_s(pid)
            cpu_before, client_before = sum(threads_before.values()), time.process_time()
            requests['n'] = requests['bytes'] = 0
            go.set()
            started = time.perf_counter()
            submitted, ends = {}, {}
            by_request = {}
            for start in range(0, agents, 1000):
                ids, sent = pipe.send_many([('submit', dict(bot=f'p.b{b}', request_id='t', prompt='script',
                                                            delivery='reject'))
                                            for b in range(start, min(agents, start + 1000))])
                for n, i in enumerate(ids):
                    by_request[i] = (start + n, sent)
            # Each turn's progress: its script's next step, by request id.
            stepping, turn_of = {}, {}
            pending = set(by_request)
            while len(ends) < agents:
                item = pipe.feed.get(timeout=600)
                assert item is not None, 'the daemon closed the session'
                at, message = item
                if 'id' in message and message['id'] in pending:
                    pending.discard(message['id'])
                    assert 'result' in message, message
                    bot, sent = by_request.pop(message['id'])
                    turn = message['result']['message']
                    submitted[turn] = sent
                    turn_of[turn] = bot
                elif 'id' in message and message['id'] in stepping:
                    assert 'result' in message, message
                    turn, n = stepping.pop(message['id'])
                    self.step(pipe, stepping, turn_of[turn], turn, n + 1)
                elif message.get('event') == 'tool_started' and steps:
                    self.step(pipe, stepping, turn_of[message['turn']], message['turn'], 0)
                elif message.get('event') == 'turn_finished':
                    assert message['data']['status'] == 'completed', message
                    ends[message['turn']] = at
            wall = time.perf_counter() - started
            threads_after = threads_cpu_s(pid)
            cpu = sum(threads_after.values()) - cpu_before
            by_thread = {k: round((v - threads_before.get(k, 0)) / wall, 2) for k, v in threads_after.items()
                         if v - threads_before.get(k, 0) > 0.01 * wall}
            client = time.process_time() - client_before
            peak_kib = peak.stop()
            stop.set()
            kind, busy = out.get(timeout=300)
            assert kind == 'busy'
            sider.join(timeout=60)
            times = [(ends[t] - submitted[t]) * 1000 for t in ends]
            # Each other bot's turn is one model request and its processor
            # time measured idle.
            cpu -= side_cpu * len(busy)
            requests['n'] -= len(busy)
            return {
                'mode': MODE + ('+steps_only' if steps and STEPS_ONLY else ''), 'agents': agents, 'calls_each': CALLS,
                'turn_ms_p50': round(percentile(times, 50), 1), 'turn_ms_p99': round(percentile(times, 99), 1),
                'turn_ms_max': round(max(times), 1), 'wall_s': round(wall, 2),
                'daemon_cpu_ms_a_turn': round(cpu * 1000 / agents, 3),
                'client_cpu_ms_a_turn': round(client * 1000 / agents, 3),
                'model_requests_a_turn': round(requests['n'] / agents, 2),
                'model_request_kib_a_turn': round(requests['bytes'] / 1024 / agents, 1),
                'rss_kib_idle': idle_kib, 'rss_kib_peak': peak_kib,
                'peak_bytes_a_bot': round((peak_kib - idle_kib) * 1024 / agents),
                'side_idle_ms_p50': round(percentile(idle, 50), 1), 'side_idle_ms_p99': round(percentile(idle, 99), 1),
                'side_busy_ms_p50': round(percentile(busy, 50), 1) if busy else None,
                'side_busy_ms_p99': round(percentile(busy, 99), 1) if busy else None,
                'side_busy_turns': len(busy),
                'cpu_share_by_thread': by_thread,
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

    @staticmethod
    def step(pipe, stepping, bot, turn, n):
        """A script's next step, or its call's answer after the last."""
        if n < CALLS:
            request, _ = pipe.send('run_step', bot=f'p.b{bot}', turn=turn, call_id='script-0', step=str(n),
                                   name='echo', arguments={'text': 'hi'})
            stepping[request] = (turn, n)
        else:
            pipe.send('tool_result', bot=f'p.b{bot}', turn=turn, call_id='script-0', output='done')

    @staticmethod
    def replies(pipe, ids):
        while ids:
            item = pipe.feed.get(timeout=120)
            assert item is not None, 'the daemon closed the session'
            message = item[1]
            if message.get('id') in ids:
                assert 'result' in message, message
                ids.discard(message['id'])


if __name__ == '__main__':
    unittest.main(argv=['code_mode_vs_direct', 'CodeModeVsDirect'])
