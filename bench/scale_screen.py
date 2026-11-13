"""How one daemon scales with the number of bots streaming at once.

N bots take text turns back to back through one socket daemon against
`bench.streaming_model`; one firehose follower (`follow *`, as a Sonny window
does) sees every event and resubmits each bot as its turn ends. Reads `stats`
at the start and end of a measured window and reports the deltas, per-thread
daemon CPU from /proc (Linux), the model's own CPU, and, from the store's own
timestamps, how long each turn waited between acceptance and its start.

    .local/venv/bin/python -m bench.scale_screen --bots 2048 --ttft-ms 300 \
        --tokens 100 --tps 50 --jitter 0.5 --out .local/bench/scale-2048

With --tpm L the model plays a provider with a tokens-per-minute limit
(429 with retry-after-ms), and --no-headers hides the limit from the pacer.
"""
import argparse
import asyncio
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import psutil

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
TICK = os.sysconf('SC_CLK_TCK')


def thread_cpu(pid):
    """CPU seconds by thread name (summed), from /proc."""
    out = {}
    for task in Path(f'/proc/{pid}/task').iterdir():
        try:
            stat = (task / 'stat').read_text()
        except OSError:
            continue
        name = stat[stat.index('(') + 1:stat.rindex(')')]
        fields = stat[stat.rindex(')') + 2:].split()
        cpu = (int(fields[11]) + int(fields[12])) / TICK
        # Group numbered workers (e.g. tokio-runtime-w) by name.
        out[name] = out.get(name, 0) + cpu
    return out


def proc_cpu(pid):
    t = psutil.Process(pid).cpu_times()
    return t.user + t.system


def pct(values, p):
    if not values:
        return None
    values = sorted(values)
    return values[min(len(values) - 1, int(p * len(values)))]


def from_store(path, window):
    """What the store's timestamps say about the window's turns: the wait
    from acceptance to start, and the pacing and retries each turn took."""
    db = sqlite3.connect(f'file:{path}?mode=ro', uri=True)
    turns = set(window)
    accepted = dict(db.execute("SELECT turn, written_ms FROM events WHERE kind='accepted'"))
    waits, paced_ms, retries, paced, retried = [], 0, 0, 0, 0
    for turn, started, pm, rt in db.execute('SELECT id, started_ms, paced_ms, retries FROM turns'):
        if turn not in turns:
            continue
        if started is not None and turn in accepted:
            waits.append(started - accepted[turn])
        paced_ms += pm or 0
        retries += rt or 0
        paced += bool(pm)
        retried += bool(rt)
    db.close()
    n = max(1, len(turns))
    return dict(start_wait_p50_ms=pct(waits, .5), start_wait_p95_ms=pct(waits, .95),
                paced_ms_per_turn=round(paced_ms / n, 1), retries_per_turn=round(retries / n, 3),
                turns_paced=paced, turns_retried=retried)


class Conn:
    def __init__(self, reader, writer):
        self.reader, self.writer = reader, writer
        self.next_id = 0
        self.waiting = {}
        self.on_event = None
        self.bytes = 0
        self.kinds = {}
        self.kind_bytes = {}

    @classmethod
    async def open(cls, path):
        r, w = await asyncio.open_unix_connection(path, limit=1 << 22)
        c = cls(r, w)
        c.task = asyncio.create_task(c.read())
        return c

    async def read(self):
        while True:
            line = await self.reader.readline()
            if not line:
                return
            self.bytes += len(line)
            m = json.loads(line)
            if 'id' in m and m['id'] in self.waiting:
                self.waiting.pop(m['id']).set_result(m)
                continue
            kind = m.get('event')
            if kind:
                self.kinds[kind] = self.kinds.get(kind, 0) + 1
                self.kind_bytes[kind] = self.kind_bytes.get(kind, 0) + len(line)
            if self.on_event:
                self.on_event(m)

    def send(self, op, **params):
        self.next_id += 1
        fut = asyncio.get_running_loop().create_future()
        self.waiting[self.next_id] = fut
        self.writer.write((json.dumps(dict(id=self.next_id, op=op, **params)) + '\n').encode())
        return fut

    async def request(self, op, **params):
        return await self.send(op, **params)


async def run(args):
    out = Path(args.out).resolve()
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    mock_cmd = [sys.executable, str(HERE / 'streaming_model.py'), '--stats', str(out / 'mock.json'),
                '--ttft-ms', str(args.ttft_ms), '--tokens', str(args.tokens), '--tps', str(args.tps),
                '--per-delta', str(args.per_delta), '--tpm', str(args.tpm), '--rpm', str(args.rpm),
                '--charge-output', str(args.charge_output)] + (['--no-headers'] if args.no_headers else []) + ['--jitter', str(args.jitter)]
    mock = subprocess.Popen(mock_cmd, stdout=subprocess.PIPE, text=True)
    port = json.loads(mock.stdout.readline())['port']
    sockdir = tempfile.mkdtemp(prefix='sc-', dir='/tmp')
    sock = f'{sockdir}/d.sock'
    workspace = out / 'ws'
    workspace.mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith(('OPENAI', 'ANTHROPIC', 'AGENT'))}
    env['HOME'] = str(out / 'home')
    daemon_cmd = [args.binary, 'serve', '--store', str(out / 'state.sqlite'),
                  '--provider', f'openai=responses,http://127.0.0.1:{port}/v1', '--socket', sock,
                  *args.extra]
    daemon = subprocess.Popen(daemon_cmd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=open(out / 'daemon.err', 'w'), env=env, start_new_session=True)
    try:
        for _ in range(500):
            if os.path.exists(sock):
                try:
                    control = await Conn.open(sock)
                    break
                except (ConnectionRefusedError, FileNotFoundError):
                    pass
            await asyncio.sleep(.01)
        else:
            raise RuntimeError('daemon did not start')
        rss_empty = psutil.Process(daemon.pid).memory_info().rss
        bots = [f'b{i}' for i in range(args.bots)]
        for i in range(0, len(bots), 256):
            futs = [control.send('create', bot=b, workspace=str(workspace), model='openai/mock',
                                 instructions='Test agent.', tools=['echo'])
                    for b in bots[i:i + 256]]
            for r in await asyncio.gather(*futs):
                assert 'result' in r, r
        await asyncio.sleep(1)
        rss_idle = psutil.Process(daemon.pid).memory_info().rss
        controls = [control] + [await Conn.open(sock) for _ in range(args.controls - 1)]
        ctl = {b: controls[i % len(controls)] for i, b in enumerate(bots)}
        fire = await Conn.open(sock)
        kinds = {'kinds': args.kinds.split(',')} if args.kinds else {}
        r = await fire.request('follow', bot='*', after=0, **kinds)
        assert 'result' in r, r
        seq = {b: 0 for b in bots}
        submitted = {}
        latencies, completed, failed = [], [0], {}
        per_bot = {b: 0 for b in bots}
        measuring = [False]
        stopping = [False]

        trace = []
        replies = []

        def submit(bot):
            seq[bot] += 1
            submitted[bot] = time.monotonic()
            fut = ctl[bot].send('submit', bot=bot, request_id=f'r{seq[bot]}', prompt=f'hello {seq[bot]}',
                         delivery='reject')
            sent = time.time() * 1000
            if measuring[0]:
                fut.add_done_callback(lambda f, sent=sent: replies.append(time.time() * 1000 - sent))

        def on_event(m):
            if m.get('event') != 'turn_finished':
                return
            bot = m['bot']
            began = submitted.pop(bot, None)
            if measuring[0]:
                trace.append(m.get('turn'))
            status = m['data']['status']
            if measuring[0]:
                if status == 'completed':
                    completed[0] += 1
                    per_bot[bot] += 1
                    if began:
                        latencies.append((time.monotonic() - began) * 1000)
                else:
                    code = m['data'].get('error') or status
                    failed[code] = failed.get(code, 0) + 1
            if not stopping[0]:
                submit(bot)
        fire.on_event = on_event
        for b in bots:
            submit(b)
        await asyncio.sleep(args.warmup)
        # Measured window.
        s0 = (await control.request('stats'))['result']
        t0 = time.monotonic()
        th0, mock0 = thread_cpu(daemon.pid), proc_cpu(mock.pid)
        me0 = time.process_time()
        fb0, fk0, fkb0 = fire.bytes, dict(fire.kinds), dict(fire.kind_bytes)
        measuring[0] = True
        rss_peak_sample = 0
        end = t0 + args.seconds
        while time.monotonic() < end:
            await asyncio.sleep(1)
            rss_peak_sample = max(rss_peak_sample, psutil.Process(daemon.pid).memory_info().rss)
        measuring[0] = False
        t1 = time.monotonic()
        th1, mock1 = thread_cpu(daemon.pid), proc_cpu(mock.pid)
        me1 = time.process_time()
        s1 = (await control.request('stats'))['result']
        fb1 = fire.bytes
        stopping[0] = True
        dt = t1 - t0

        def d(path):
            a, b = s0, s1
            for k in path.split('.'):
                a, b = a[k], b[k]
            return b - a
        threads = {k: round(th1.get(k, 0) - th0.get(k, 0), 3) for k in th1}
        threads = {k: v for k, v in sorted(threads.items(), key=lambda kv: -kv[1]) if v > 0}
        daemon_cpu = (d('process.cpu_user_ms') + d('process.cpu_system_ms')) / 1000
        timed = d('tokens.timed_rounds')
        jobs = d('store.jobs')
        commits = d('store.groups.count')
        ops = {}
        for name, o in s1['store']['operations'].items():
            c = o['count'] - s0['store']['operations'].get(name, {}).get('count', 0)
            if c:
                ops[name] = c
        cmds = {}
        for name, o in s1['commands']['ops'].items():
            o0 = s0['commands']['ops'].get(name, {'count': 0, 'held_ms': 0})
            if o['count'] - o0['count']:
                cmds[name] = dict(count=o['count'] - o0['count'], held_ms=o['held_ms'] - o0['held_ms'],
                                  slowest_ms=o['slowest_ms'])
        kinds = {k: fire.kinds.get(k, 0) - fk0.get(k, 0) for k in fire.kinds}
        kind_bytes = {k: fire.kind_bytes.get(k, 0) - fkb0.get(k, 0) for k in fire.kind_bytes}
        mstats = json.loads((out / 'mock.json').read_text())
        spread = sorted(per_bot.values())
        pools = s1['providers']['openai']['pools']
        result = dict(
            bots=args.bots, seconds=round(dt, 2),
            turns=completed[0], failed=failed, turns_per_s=round(completed[0] / dt, 2),
            output_tps=round(d('tokens.output_tokens') / dt, 1),
            input_tps=round(d('tokens.input_tokens') / dt, 1),
            call_tps=round(d('tokens.timed_output_tokens') / max(1, d('tokens.model_ms')) * 1000, 1),
            first_token_ms=round(d('tokens.first_token_ms') / max(1, timed), 1),
            model_ms=round(d('tokens.model_ms') / max(1, timed), 1),
            daemon_ms_per_call=round(d('tokens.daemon_ms') / max(1, timed), 2),
            timed_rounds=timed,
            latency_p50_ms=round(pct(latencies, .5) or 0), latency_p95_ms=round(pct(latencies, .95) or 0),
            latency_max_ms=round(max(latencies) if latencies else 0),
            ideal_ms=round(args.ttft_ms + args.tokens / args.tps * 1000),
            daemon_cpu_pct=round(daemon_cpu / dt * 100, 1),
            daemon_cpu_ms_per_turn=round(daemon_cpu * 1000 / max(1, completed[0]), 3),
            threads_cpu_s=threads,
            mock_cpu_pct=round((mock1 - mock0) / dt * 100, 1),
            driver_cpu_pct=round((me1 - me0) / dt * 100, 1),
            runtime_thread_pct=round(sum(v for k, v in threads.items() if k.startswith('agent') and not k.startswith('agent-storage')) / dt * 100, 1),
            writer_thread_pct=round(threads.get('agent-storage', 0) / dt * 100, 1),
            rss_empty_mib=round(rss_empty / 2**20, 1), rss_idle_mib=round(rss_idle / 2**20, 1),
            rss_mib=round(s1['process']['rss_bytes'] / 2**20, 1),
            rss_peak_mib=round(s1['process']['peak_rss_bytes'] / 2**20, 1),
            threads=s1['process']['threads'],
            store_jobs_per_turn=round(jobs / max(1, completed[0]), 2),
            commits_per_s=round(commits / dt, 1), jobs_per_commit=round(jobs / max(1, commits), 2),
            store_ran_ms_per_turn=round(d('store.ran_ms') / max(1, completed[0]), 3),
            store_queued_ms_per_job=round(d('store.queued_ms') / max(1, jobs), 3),
            store_ops=ops,
            store_mib=round(s1['store']['bytes'] / 2**20, 1), wal_mib=round(s1['store']['wal_bytes'] / 2**20, 1),
            commands=cmds,
            follower_events_per_s=round(sum(kinds.values()) / dt, 1),
            follower_kib_per_s=round((fb1 - fb0) / dt / 1024, 1),
            follower_kinds=kinds, follower_kind_bytes=kind_bytes,
            per_bot_turns_min=spread[0], per_bot_turns_max=spread[-1],
            mock=mstats, pools=pools, paced_turns=s1['paced_turns'],
            transport=s1['transport'],
        )
        result['submit_reply_p50_ms'] = round(pct(replies, .5) or 0, 1)
        result['submit_reply_p95_ms'] = round(pct(replies, .95) or 0, 1)
        (out / 'stats0.json').write_text(json.dumps(s0))
        (out / 'stats1.json').write_text(json.dumps(s1))
        # Let in-flight turns end before shutdown.
        deadline = time.monotonic() + 60
        while submitted and time.monotonic() < deadline:
            await asyncio.sleep(.2)
        await control.request('shutdown')
        daemon.wait(timeout=30)
        result.update(from_store(out / 'state.sqlite', trace))
        return result
    finally:
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
        mock.terminate()
        mock.wait()
        shutil.rmtree(sockdir, ignore_errors=True)


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--binary', default=str(ROOT / '.local/target/release/agent'))
    p.add_argument('--out', required=True)
    p.add_argument('--bots', type=int, default=16)
    p.add_argument('--warmup', type=float, default=8)
    p.add_argument('--seconds', type=float, default=30)
    p.add_argument('--ttft-ms', type=float, default=300)
    p.add_argument('--tokens', type=int, default=100)
    p.add_argument('--tps', type=float, default=50)
    p.add_argument('--per-delta', type=int, default=5)
    p.add_argument('--tpm', type=int, default=0)
    p.add_argument('--rpm', type=int, default=0)
    p.add_argument('--charge-output', type=int, default=100)
    p.add_argument('--kinds', default='')
    p.add_argument('--no-headers', action='store_true')
    p.add_argument('--controls', type=int, default=1)
    p.add_argument('--jitter', type=float, default=0)
    p.add_argument('extra', nargs='*')
    a = p.parse_args()
    try:
        import uvloop
        uvloop.install()
    except ImportError:
        pass
    result = asyncio.run(run(a))
    Path(a.out, 'result.json').write_text(json.dumps(result, indent=2))
    brief = {k: v for k, v in result.items() if k not in ('store_ops', 'commands', 'follower_kinds',
                                                           'follower_kind_bytes', 'pools', 'transport', 'threads_cpu_s')}
    print(json.dumps(brief))


if __name__ == '__main__':
    main()
