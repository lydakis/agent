"""The 10,000-bot shape: many bots exist, a bounded set is active, most wait.

Runs through the daemon's stdio protocol, never CLI processes. Phases, each
with daemon RSS, threads, and open files sampled from outside:

1. create N bots;
2. submit every bot once with --max-active bounding the live set; submissions
   the daemon refuses at capacity are retried as turns finish, so the screen
   records acceptance, refusals, throughput at the bound, and latency;
3. park: P bots wait on one anchor turn the synthetic model holds open, then
   the anchor is released and the parked turns drain through the bound;
4. restart: submit a bounded wave held open by the synthetic provider, kill
   the daemon, confirm its exit, and time recovery on the same store.

Synthetic by default (no spend). With --model PROVIDER/MODEL it runs phase 2
against a real provider with the key from the environment; the shell-tool
prompt from the fleet check is used there.

    .local/venv/bin/python -m bench.fleet_screen --bots 10000 --max-active 1024 \
        --out .local/bench/fleet-screen-10k
"""
import argparse
import json
import os
import queue
import signal
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import psutil

from .runtime_client import Client
from .synthetic_model import start
from .targets import file_hash

LIVE_PROMPT = 'Run wc -l notes.txt with the shell tool and report just the number.'
ENDPOINTS = {'openai': ('responses', 'https://api.openai.com/v1', 'OPENAI_API_KEY'),
             'anthropic': ('anthropic', 'https://api.anthropic.com/v1', 'ANTHROPIC_API_KEY')}


def percentile(values, p):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, int(len(ordered) * p) - 1))] if ordered else None


class Screen:
    def __init__(self, binary, out, bots, max_active, model, parked, delay_ms, restart=True):
        if not model and parked > 0 and max_active == 1:
            raise ValueError('parking requires --max-active of at least 2 (or 0 for unbounded)')
        self.binary, self.out, self.bots, self.max_active, self.parked = binary, out, bots, max_active, parked
        self.delay_ms, self.with_restart = delay_ms, restart
        self.store = out / 'state.sqlite'
        self.workspace = out / 'workspace'
        self.workspace.mkdir(parents=True)
        (self.workspace / 'notes.txt').write_text('alpha\nbeta\ngamma\n')
        self.model = model
        self.env = os.environ.copy()
        if model:
            provider, name = model.split('/', 1)
            self.family, self.url, key_env = ENDPOINTS[provider]
            self.provider, self.name = provider, name
            if key_env not in os.environ:
                raise SystemExit(f'{key_env} is not set')
            self.key_env, self.server = key_env, None
        else:
            self.server, self.url = start()
            self.provider, self.family, self.name, self.key_env = 'openai', 'responses', 'synthetic-model', None
        self.client = None
        self.phases = {}
        self.finished, self.waiting = {}, set()

    def daemon(self):
        extra = ['--max-active', str(self.max_active), '--context-items', '8']
        self.client = Client(self.binary, self.store, self.url, 'shell,read,write,edit,wait', model=self.name,
                             key_env=self.key_env, env=self.env, provider=self.provider, family=self.family, extra=extra)
        self.process = psutil.Process(self.client.process.pid)
        return self.client

    def sample(self, label):
        try:
            info = self.process.memory_info()
            return dict(label=label, rss_mib=round(info.rss / 2**20, 2), threads=self.process.num_threads(),
                        fds=self.process.num_fds(),
                        store_mib=round(sum(p.stat().st_size for p in self.out.glob('state.sqlite*')) / 2**20, 2))
        except psutil.Error:
            return dict(label=label)

    def absorb(self, message):
        if message is None:
            raise RuntimeError('daemon exited')
        if message.get('event') == 'turn_finished':
            self.finished[message['turn']] = message
        elif message.get('event') == 'turn_waiting':
            self.waiting.add(message['turn'])

    def pump(self, timeout=.05):
        """Absorb every queued daemon message, blocking up to `timeout` for one."""
        for message in self.client.saved:
            self.absorb(message)
        self.client.saved.clear()
        try:
            self.absorb(self.client.queue.get(timeout=timeout))
        except queue.Empty:
            return
        while True:
            try:
                self.absorb(self.client.queue.get_nowait())
            except queue.Empty:
                return

    def create_all(self):
        started = time.monotonic()
        for index in range(self.bots):
            response = self.client.request('create', bot=f'b{index}', workspace=str(self.workspace), reasoning='low')
            if 'error' in response:
                raise RuntimeError(f"create failed: {response['error']}")
        seconds = time.monotonic() - started
        self.phases['create'] = dict(bots=self.bots, seconds=round(seconds, 2),
                                     per_second=round(self.bots / seconds), sample=self.sample('after create'))

    def run_all(self, label, prompt):
        """Submit every bot once, retrying refusals as turns finish."""
        pending = list(range(self.bots))
        in_flight, done, completed, refused, latency, samples = {}, 0, 0, {}, [], []
        started = time.monotonic()
        next_sample = started
        submission_limit = self.max_active or self.bots

        def submit(index):
            submitted_at = time.monotonic()
            response = self.client.request('submit', bot=f'b{index}', request_id=f'{label}', prompt=prompt)
            if 'error' in response:
                refused[response['error']] = refused.get(response['error'], 0) + 1
                pending.append(index)
                return False
            in_flight[response['result']['turn']] = submitted_at
            return True

        # Fill to the bound, then keep it full as turns finish. A refusal
        # just means the bound is full; the bot goes back to the end of the line.
        while pending or in_flight:
            while pending and len(in_flight) < submission_limit:
                if not submit(pending.pop(0)):
                    break  # the bound is full; wait for a finish before trying again
            self.pump()
            for turn in [t for t in in_flight if t in self.finished]:
                event = self.finished.pop(turn)
                latency.append((event['_received_at'] - in_flight.pop(turn)) * 1000)
                done += 1
                data = event['data']
                if data['status'] == 'completed':
                    completed += 1
                else:
                    code = 'finished:' + (data.get('error') or data['status'])
                    refused[code] = refused.get(code, 0) + 1
            now = time.monotonic()
            if now >= next_sample:
                samples.append(dict(t_s=round(now - started, 1), in_flight=len(in_flight), done=done,
                                    **self.sample('run')))
                next_sample = now + 2
        seconds = time.monotonic() - started
        return dict(seconds=round(seconds, 2), finished=done, completed=completed, failed=done - completed,
                    turns_per_second=round(completed / seconds, 1) if seconds else None,
                    finished_per_second=round(done / seconds, 1) if seconds else None,
                    refusals=refused, p50_ms=round(percentile(latency, .5) or 0), p95_ms=round(percentile(latency, .95) or 0),
                    max_ms=round(max(latency) if latency else 0), peak_in_flight=max((s['in_flight'] for s in samples), default=0),
                    samples=samples)

    def park(self):
        anchor = self.client.request('submit', bot='b0', request_id='anchor', prompt='hold:')['result']['turn']
        started = time.monotonic()
        turns, refused = set(), {}
        pending = list(range(1, self.parked + 1))
        # A parked turn needs an active slot for the model call that parks
        # it, so refusals at the bound are retried as earlier ones park.
        while pending:
            response = self.client.request('submit', bot=f'b{pending[0]}', request_id='park', prompt=f'wait:turn:b0/{anchor}')
            if 'error' in response:
                refused[response['error']] = refused.get(response['error'], 0) + 1
                self.pump(.02)
                continue
            pending.pop(0)
            turns.add(response['result']['turn'])
        deadline = time.monotonic() + 300
        while not turns <= self.waiting and time.monotonic() < deadline:
            self.pump(.1)
        parked = len(turns & self.waiting)
        parked_sample = self.sample('parked')
        park_seconds = time.monotonic() - started
        self.server.release.set()
        released = time.monotonic()
        wanted = turns | {anchor}
        while not wanted <= set(self.finished) and time.monotonic() - released < 600:
            self.pump(.1)
        finished = len(wanted & set(self.finished))
        for turn in wanted:
            self.finished.pop(turn, None)
        drain_seconds = time.monotonic() - released
        self.phases['park'] = dict(parked=parked, refused=refused, park_seconds=round(park_seconds, 2),
                                   parked_sample=parked_sample, drain_seconds=round(drain_seconds, 2),
                                   finished=finished, after=self.sample('after drain'))

    def restart(self):
        # Hold every submitted turn until after the old process exits. This
        # guarantees live recovery work even with one bot or instant replies.
        active = min(self.max_active or self.bots, self.bots)
        self.server.release.clear()
        try:
            for index in range(active):
                response = self.client.request('submit', bot=f'b{index}', request_id='restart', prompt='hold:')
                if 'error' in response:
                    raise RuntimeError(f"restart submission failed: {response['error']}")
            before_kill = self.sample('before kill')
            self.client.process.send_signal(signal.SIGKILL)
            self.client.close(kill=True)  # waits for exit and releases the store
        finally:
            self.server.release.set()
        started = time.monotonic()
        client = self.daemon()
        ready_seconds = time.monotonic() - started
        statuses = {}
        for index in range(self.bots):
            state = client.request('resume', bot=f'b{index}')['result']
            statuses[state['status']] = statuses.get(state['status'], 0) + 1
        if statuses.get('interrupted', 0) != active:
            raise RuntimeError(f'restart recovered {statuses}, expected {active} interrupted bots')
        self.phases['restart'] = dict(in_flight_at_kill=active,
                                      before_kill=before_kill, ready_seconds=round(ready_seconds, 3),
                                      bot_statuses=statuses, sample=self.sample('after restart'))

    def run(self):
        self.daemon()
        self.phases['start'] = self.sample('ready')
        self.create_all()
        print('created', self.phases['create'], file=sys.stderr)
        prompt = LIVE_PROMPT if self.model else f'delay:{self.delay_ms}'
        self.phases['burst'] = self.run_all('burst', prompt)
        print('burst', {k: v for k, v in self.phases['burst'].items() if k != 'samples'}, file=sys.stderr)
        if not self.model:
            if self.parked:
                self.park()
                print('park', self.phases['park'], file=sys.stderr)
            if self.with_restart:
                self.restart()
                print('restart', self.phases['restart'], file=sys.stderr)
        result = dict(schema='fleet_screen_v4', created_at=datetime.now(timezone.utc).isoformat(),
                      binary_sha256=file_hash(self.binary), bots=self.bots, max_active=self.max_active,
                      model=self.model or 'synthetic', parked=self.parked if not self.model else 0,
                      delay_ms=self.delay_ms if not self.model else None,
                      provider_requests=self.server.requests if self.server else None, phases=self.phases,
                      host=dict(system=os.uname().sysname, machine=os.uname().machine))
        self.client.request('shutdown')
        self.client.close()
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--bots', type=int, default=10000)
    parser.add_argument('--max-active', type=int, default=1024)
    parser.add_argument('--parked', type=int, default=5000, help='bots parked on one anchor (synthetic only)')
    parser.add_argument('--model', default=None, help='PROVIDER/MODEL for a paid burst; synthetic when omitted')
    parser.add_argument('--delay-ms', type=int, default=500, help='synthetic reply delay so turns overlap at the bound')
    parser.add_argument('--no-restart', action='store_true', help='skip the kill-and-recover phase (a heap profile is written only at a clean exit)')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    out = args.out.resolve()
    if Path.cwd() != root or not out.is_relative_to(root / '.local') or out.exists():
        parser.error('run from the repository root with a new output directory under .local')
    if not (1 <= args.bots <= 100000) or args.parked >= args.bots:
        parser.error('--bots must be 1 to 100000 and --parked below it')
    try:
        screen = Screen(args.binary.resolve(), out, args.bots, args.max_active, args.model, args.parked, args.delay_ms,
                        restart=not args.no_restart)
    except ValueError as error:
        parser.error(str(error))
    result = screen.run()
    (out / 'result.json').write_text(json.dumps(result, indent=2))
    summary = {k: v for k, v in result.items() if k != 'phases'}
    summary['phases'] = {name: {k: v for k, v in phase.items() if k != 'samples'} for name, phase in result['phases'].items()}
    print(json.dumps(summary))
    return 0


if __name__ == '__main__':
    sys.exit(main())
