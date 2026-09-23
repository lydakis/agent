"""A mixed-workload soak: everything at once, for as long as asked.

One daemon on the socket transport, driven over a control connection with
every bot followed on a second one, the synthetic provider, no spend. Bots play roles that run concurrently for the whole
run: long histories with compaction on a small window, shell output that
overflows into artifacts, background-command bursts, parents parked on
children, one-time provider failures, historical forks that are run and
deleted, slow socket followers, and a running turn interrupted every
30 seconds (a slow turn is submitted for it when nothing is running).
Retention prunes every bot as it goes.

Sampled every few seconds: daemon RSS, threads, descendants, store and WAL
size, the store's queue and run time, active, waiting, paced, and queued
turns, background work, provider pools, and follower disconnects. Counted
throughout: turns by outcome, compactions, forks, deletes, cancellation
latency, observed follower disconnects, and provider failures. At the end the fast
followers' durable streams are checked against replay, then the daemon is
killed with turns in flight and restarted on the same store.

    .local/venv/bin/python -m bench.soak --minutes 60 --out .local/bench/soak-60
"""
import argparse
import json
import queue
import random
import signal
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

import psutil

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.fleet_screen import percentile  # noqa: E402
from bench.runtime_client import Client  # noqa: E402
from bench.socket_client import Connection  # noqa: E402
from bench.soak_followers import JournalFollower, SlowFollower  # noqa: E402
from bench.synthetic_model import start  # noqa: E402
from bench.targets import file_hash  # noqa: E402

COMPACTION = ('Merge any earlier summary with the turns below. Keep the goal, every rule the user stated, what is '
              'done, and next steps. Reply with the summary only.')
ROLES = {'long': 32, 'noisy': 32, 'background': 32, 'parent': 16, 'child': 16, 'flaky': 16, 'plain': 48}
# Seconds between a bot's turns, so an hour stays within a few gigabytes of
# store and the fleet runs at tens of turns per second, not hundreds.
PACE = {'long': 2, 'noisy': 10, 'background': 3, 'parent': 3, 'child': 0, 'flaky': 5, 'plain': 2, 'fork': 1}


class Soak:
    def __init__(self, binary, out, minutes, seed):
        self.binary, self.out, self.minutes = binary, out, minutes
        self.random = random.Random(seed)
        self.store = out / 'state.sqlite'
        self.directory = tempfile.TemporaryDirectory(prefix='agent-soak-', dir='/tmp')
        self.socket = Path(self.directory.name) / 'daemon.sock'
        self.connections = []
        self.workspace = out / 'workspace'
        self.workspace.mkdir(parents=True)
        self.server, self.url = start()
        self.client = None
        self.bots = [(role, f'{role}{i}') for role, count in ROLES.items() for i in range(count)]
        for role, name in self.bots:
            (self.workspace / name).mkdir()
        self.finished, self.terminal_at = {}, {}
        self.counts = {'turns': 0, 'completed': 0, 'failed': 0, 'interrupted': 0, 'interrupt_attempts': 0, 'compactions': 0, 'forks': 0,
                       'deletes': 0, 'retries': 0, 'follower_disconnects': 0, 'refusals': {}}
        self.cancel_ms, self.turn_ms, self.samples, self.errors = [], [], [], {}
        self.in_flight = {}  # turn -> (role, bot, submitted_at)
        self.turn_counter = {}
        self.next_at = {}
        self.followers, self.slow = {}, []
        self.forks = []

    # ---- daemon ----
    def daemon(self):
        extra = ('--socket', str(self.socket), '--max-active', '256', '--context-bytes', str(512 * 1024),
                 '--compact-at', '50', '--compact-keep', '25', '--retain-turns', '12')
        # A socket daemon answers on the socket; the stdio handle only owns the process.
        self.client = Client(self.binary, self.store, self.url, 'shell,read,write,edit,wait,history',
                             extra=extra)
        self.process = psutil.Process(self.client.process.pid)
        self.control = Connection(self.socket, retain_durable=False)
        self.connections.append(self.control)
        self.control.tools = ['shell', 'read', 'write', 'edit', 'wait', 'history']
        self.events = Connection(self.socket, retain_durable=False)
        self.connections.append(self.events)
        return self.control

    def follow(self, name):
        self.events.request('follow', bot=name, after=0)

    def create_all(self):
        for role, name in self.bots:
            kwargs = {'compaction_instructions': COMPACTION} if role == 'long' else {}
            response = self.control.request('create', bot=name, workspace=str(self.workspace / name), **kwargs)
            if 'error' in response:
                raise RuntimeError(f"create {name}: {response['error']}")
            self.follow(name)

    # ---- events ----
    def absorb(self, message):
        if message is None:
            raise RuntimeError('daemon exited')
        event = message.get('event')
        if event == 'turn_finished':
            self.finished[message['turn']] = message
        elif event == 'compacted':
            self.counts['compactions'] += 1
        elif event == 'retry':
            self.counts['retries'] += 1

    def pump(self, timeout=.02):
        for message in self.events.saved:
            self.absorb(message)
        self.events.saved.clear()
        try:
            self.absorb(self.events.queue.get(timeout=timeout))
        except queue.Empty:
            return
        while True:
            try:
                self.absorb(self.events.queue.get_nowait())
            except queue.Empty:
                return

    # ---- work ----
    def prompt(self, role, bot):
        n = self.turn_counter[bot] = self.turn_counter.get(bot, 0) + 1
        if role == 'long':
            return f'text:{self.random.choice([512, 2048, 4096])} turn {n} ' + 'l' * self.random.choice([1024, 3072])
        if role == 'noisy':
            # Mostly under the 64 KiB preview, sometimes well over it.
            size = self.random.choice([20_000, 60_000, 200_000, 1_000_000])
            return f"shell:head -c {size} /dev/zero | tr '\\0' n"
        if role == 'background':
            return 'bgwait:sleep 0.2; printf done'
        if role == 'flaky':
            return f'flaky:{bot}-{n}'
        return f'delay:{self.random.choice([20, 100, 300])}'

    def submit(self, role, bot, prompt, **extra):
        submitted_at = time.monotonic()
        response = self.control.request('submit', bot=bot, request_id=f't{self.turn_counter.get(bot, 0)}-{time.monotonic_ns()}',
                                        prompt=prompt, **extra)
        if 'error' in response:
            self.counts['refusals'][response['error']] = self.counts['refusals'].get(response['error'], 0) + 1
            return None
        turn = response['result']['turn']
        self.in_flight[turn] = (role, bot, submitted_at)
        self.next_at[bot] = time.monotonic() + PACE.get(role, 1)
        return turn

    def keep_busy(self):
        """Every bot has at most one turn in flight; refill as they finish."""
        busy = {bot for _, bot, _ in self.in_flight.values()}
        now = time.monotonic()
        for role, bot in self.bots:
            if bot in busy or role == 'child' or self.next_at.get(bot, 0) > now:
                continue
            if role == 'parent':
                child = bot.replace('parent', 'child')
                if child in busy:
                    continue
                child_turn = self.submit('child', child, f'delay:{self.random.choice([500, 1500, 3000])}')
                if child_turn is None:
                    continue
                self.submit(role, bot, f'wait:turn:{child}/{child_turn}')
                continue
            self.submit(role, bot, self.prompt(role, bot))

    def collect(self):
        for turn in [t for t in self.in_flight if t in self.finished]:
            event = self.finished.pop(turn)
            role, bot, at = self.in_flight.pop(turn)
            status = event['data']['status']
            self.counts['turns'] += 1
            self.turn_ms.append((event['_received_at'] - at) * 1000)
            if status == 'completed':
                self.counts['completed'] += 1
            elif status == 'interrupted':
                self.counts['interrupted'] += 1
                if turn in self.terminal_at:
                    self.cancel_ms.append((event['_received_at'] - self.terminal_at.pop(turn)) * 1000)
            else:
                self.counts['failed'] += 1
                key = f"{role}:{event['data'].get('error')}"
                self.errors[key] = self.errors.get(key, 0) + 1

    def interrupt_one(self):
        running = [(t, r, b) for t, (r, b, _) in self.in_flight.items() if r in ('noisy', 'plain', 'long', 'background')]
        if running:
            turn, _, bot = self.random.choice(running)
        else:
            # The synthetic provider answers in milliseconds, so nothing may be
            # running at this instant; give an idle plain bot a slow turn.
            busy = {b for _, b, _ in self.in_flight.values()}
            idle = [b for r, b in self.bots if r == 'plain' and b not in busy]
            if not idle:
                return
            bot = self.random.choice(idle)
            turn = self.submit('plain', bot, 'delay:5000')
            if turn is None:
                return
        self.terminal_at[turn] = time.monotonic()
        self.counts['interrupt_attempts'] += 1
        response = self.control.request('interrupt', bot=bot, turn=turn)
        if 'result' not in response:
            key = f"interrupt:{response.get('error')}"
            self.errors[key] = self.errors.get(key, 0) + 1

    def fork_one(self):
        """Fork a long bot at an old checkpoint, run it twice, delete it."""
        _, source = self.random.choice([b for b in self.bots if b[0] == 'long'])
        page = self.control.request('events', bot=source, after=0, limit=64)['result']['events']
        checkpoints = [e['data']['checkpoint'] for e in page if e['event'] == 'turn_finished' and e['data'].get('checkpoint')]
        if not checkpoints:
            return
        name = f'fork{self.counts["forks"]}'
        response = self.control.request('fork', source=source, checkpoint=checkpoints[0], bot=name,
                                        workspace=str(self.workspace / source))
        if 'error' in response:
            self.counts['refusals']['fork:' + response['error']] = self.counts['refusals'].get('fork:' + response['error'], 0) + 1
            return
        self.counts['forks'] += 1
        self.forks.append(name)
        self.bots.append(('fork', name))
        self.follow(name)

    def retire_forks(self):
        for name in list(self.forks):
            if any(b == name for _, b, _ in self.in_flight.values()):
                continue
            if self.turn_counter.get(name, 0) < 2:
                self.turn_counter[name] = self.turn_counter.get(name, 0) + 1
                self.submit('fork', name, f'delay:{self.random.choice([50, 200])}')
                continue
            response = self.control.request('delete', bot=name)
            if 'result' in response:
                self.counts['deletes'] += 1
                self.forks.remove(name)
                self.bots = [b for b in self.bots if b[1] != name]

    def attach_followers(self):
        for role, name in self.bots[:8]:
            follower = JournalFollower(self.socket, name)
            self.connections.append(follower)
            self.followers[name] = follower
        for role, name in [b for b in self.bots if b[0] == 'noisy'][:4]:
            follower = SlowFollower(self.socket, name, pause=.05)
            self.connections.append(follower)
            self.slow.append((name, follower))

    # ---- sampling ----
    def sample(self, t0):
        stats = self.control.request('stats')['result']
        store = stats['store']
        descendants = self.process.children(recursive=True)
        descendant_rss = 0
        for child in descendants:
            try:
                descendant_rss += child.memory_info().rss
            except psutil.Error:
                pass  # a shell child that exited between the listing and the read
        entry = {'t_s': round(time.monotonic() - t0, 1), 'rss_mib': round(self.process.memory_info().rss / 2**20, 2),
                 'threads': self.process.num_threads(), 'fds': self.process.num_fds(),
                 'descendants': len(descendants),
                 'descendant_rss_mib': round(descendant_rss / 2**20, 2),
                 'store_mib': round(self.store.stat().st_size / 2**20, 2),
                 'wal_mib': round((self.store.with_name('state.sqlite-wal').stat().st_size if self.store.with_name('state.sqlite-wal').exists() else 0) / 2**20, 2),
                 'active': stats['active_turns'], 'waiting': stats['waiting_turns'], 'paced': stats['paced_turns'],
                 'queued': stats['queued_turns'], 'processes': stats['running_processes'],
                 'store_jobs': store['jobs'], 'store_queued_ms': store['queued_ms'], 'store_ran_ms': store['ran_ms'],
                 'in_flight': len(self.in_flight), 'turns': self.counts['turns'], 'compactions': self.counts['compactions'],
                 'sessions': stats['sessions'], 'pools': {k: v.get('pools') for k, v in stats['providers'].items()}}
        self.samples.append(entry)
        return entry

    # ---- phases ----
    def close_connections(self):
        for connection in self.connections:
            connection.close()
        self.connections.clear()

    def close(self):
        self.server.release.set()
        try:
            if self.client:
                self.client.close(kill=True)
        finally:
            self.close_connections()
            self.server.shutdown()
            self.server.server_close()
            self.directory.cleanup()

    def run(self):
        try:
            return self.run_phases()
        finally:
            self.close()

    def run_phases(self):
        self.daemon()
        self.create_all()
        self.attach_followers()
        t0 = time.monotonic()
        deadline = t0 + self.minutes * 60
        next_sample, next_interrupt, next_fork = t0 + 5, t0 + 10, t0 + 60
        while time.monotonic() < deadline:
            self.keep_busy()
            self.pump()
            self.collect()
            self.retire_forks()
            now = time.monotonic()
            if now >= next_sample:
                entry = self.sample(t0)
                print(json.dumps({k: entry[k] for k in ('t_s', 'rss_mib', 'store_mib', 'wal_mib', 'active', 'waiting',
                                                         'in_flight', 'turns', 'compactions', 'descendants')}),
                      file=sys.stderr, flush=True)
                next_sample = now + 5
            if now >= next_interrupt:
                self.interrupt_one()
                next_interrupt = now + 30
            if now >= next_fork:
                self.fork_one()
                next_fork = now + 60
        # Drain what is in flight, then check the fast followers against replay.
        drain_until = time.monotonic() + 120
        while self.in_flight and time.monotonic() < drain_until:
            self.pump(.1)
            self.collect()
            self.retire_forks()
        final = self.sample(t0)
        unfinished = sorted(f'{role}:{bot}' for role, bot, _ in self.in_flight.values())
        mismatches = sum(not follower.matches(self.control, name)
                         for name, follower in self.followers.items())
        slow_status = [{'bot': name, 'bytes_read': follower.bytes_read,
                        'disconnected': follower.disconnected} for name, follower in self.slow]
        self.counts['follower_disconnects'] = sum(f['disconnected'] for f in slow_status)
        # Restart with work in flight.
        self.server.release.clear()
        held = [self.submit('plain', bot, 'hold:') for _, bot in [b for b in self.bots if b[0] == 'plain'][:16]]
        time.sleep(1)
        self.client.process.send_signal(signal.SIGKILL)
        self.client.close(kill=True)
        self.close_connections()
        self.server.release.set()
        started = time.monotonic()
        self.daemon()
        ready_s = round(time.monotonic() - started, 3)
        statuses = {}
        for _, name in self.bots:
            state = self.control.request('resume', bot=name)['result']
            statuses[state['status']] = statuses.get(state['status'], 0) + 1
        self.control.request('shutdown')
        self.client.close()
        return {'schema': 'soak_v3', 'created_at': datetime.now(timezone.utc).isoformat(),
                'binary_sha256': file_hash(self.binary), 'minutes': self.minutes, 'roles': ROLES,
                'counts': self.counts, 'errors': self.errors, 'provider_requests': self.server.requests,
                'turn_ms': {'boundary': 'before_submit_to_terminal_receive',
                            'p50': round(percentile(self.turn_ms, .5) or 0), 'p95': round(percentile(self.turn_ms, .95) or 0),
                            'max': round(max(self.turn_ms) if self.turn_ms else 0), 'n': len(self.turn_ms)},
                'cancel_ms': {'p50': round(percentile(self.cancel_ms, .5) or 0), 'max': round(max(self.cancel_ms) if self.cancel_ms else 0),
                              'n': len(self.cancel_ms)},
                'follower_mismatches': mismatches, 'slow_followers': slow_status, 'in_flight_at_end': len(self.in_flight),
                'unfinished_after_drain': unfinished,
                'restart': {'held': len([h for h in held if h]), 'ready_s': ready_s, 'bot_statuses': statuses},
                'final_sample': final, 'samples': self.samples}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--minutes', type=float, default=60)
    parser.add_argument('--seed', type=int, default=7)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    result = Soak(args.binary.resolve(), out, args.minutes, args.seed).run()
    (out / 'result.json').write_text(json.dumps(result, indent=1))
    print(json.dumps({k: v for k, v in result.items() if k != 'samples'}, indent=1))


if __name__ == '__main__':
    main()
