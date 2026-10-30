"""How the lookout and the approver scale with the number of bots.

One socket daemon on a streaming synthetic model, P projects of T tasks
each (every project's lead subscribed to its tasks, as Sonny does), and the
companions in a bot's path: `sonny lookout` and `agent approver`. Phases:

- idle: the lookout's start (until every lead is seated), RSS and CPU idle;
- load: every task runs back-to-back turns that stream K deltas MS apart;
  lookout and daemon CPU, how late each lead hears that a turn ended, and
  whether the daemon dropped the lookout for falling behind;
- restart: the lookout started again over the whole log, until it is live;
- approver: every task makes one gated shell call at once; how many the
  approver allowed, and how many timed out, at a judge that takes J ms.

Run from the repo root after `cargo build --release --workspace --bins`:
    python3 -m bench.lookout_scale --projects 10 --tasks 10 --out /tmp/x
The model runs as its own process (`--model PORTFILE`), so its CPU is not
counted against the companions.
"""
import argparse
import asyncio
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

TICK = os.sysconf('SC_CLK_TCK')
PAGE = os.sysconf('SC_PAGE_SIZE')


# The model -----------------------------------------------------------------

def user_text(request):
    users = [i for i in request.get('input', []) if i.get('role') == 'user']
    if not users:
        return ''
    content = users[-1]['content']
    if isinstance(content, str):
        return content
    return ''.join(part.get('text', '') for part in content)


async def respond(request, writer, judge_ms):
    text = user_text(request)
    last = (request.get('input') or [{}])[-1]
    after_tool = last.get('type') == 'function_call_output'
    deltas, gap, output = ['ok'], 0.0, None
    if text.startswith('{"state"'):
        await asyncio.sleep(judge_ms / 1000)
        answer = json.dumps({id: {'outcome': 'allow'} for id in json.loads(text)['assess']})
        deltas = [answer]
    elif re.match(r'stream:\d+:\d+', text) and not after_tool:
        count, ms = re.match(r'stream:(\d+):(\d+)', text).groups()
        deltas, gap = ['word '] * int(count), int(ms) / 1000
    elif text.startswith('shell:') and not after_tool:
        deltas = []
        output = [{'type': 'function_call', 'name': 'shell', 'call_id': 'sh-1',
                   'arguments': json.dumps({'command': text[6:], 'timeout_ms': 5000})}]
    if os.environ.get('LS_MODEL_LOG'):
        with open(os.environ['LS_MODEL_LOG'], 'a') as log:
            log.write(json.dumps({'text': text[:200], 'last': str(last)[:200],
                                  'tools': [t.get('name') for t in request.get('tools', [])]}) + '\n')
    whole = ''.join(deltas)
    if output is None:
        output = [{'type': 'message', 'role': 'assistant',
                   'content': [{'type': 'output_text', 'text': whole}]}]
    writer.write(b'HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n'
                 b'Transfer-Encoding: chunked\r\n\r\n')

    async def frame(event):
        data = ('data: ' + json.dumps(event) + '\n\n').encode()
        writer.write(f'{len(data):x}\r\n'.encode() + data + b'\r\n')
        await writer.drain()

    await frame({'type': 'response.created', 'response': {'id': 'r'}})
    for delta in deltas:
        if gap:
            await asyncio.sleep(gap)
        await frame({'type': 'response.output_text.delta', 'delta': delta})
    await frame({'type': 'response.completed', 'response': {
        'status': 'completed', 'output': output,
        'usage': {'input_tokens': 1, 'output_tokens': 1,
                  'input_tokens_details': {'cached_tokens': 0}}}})
    writer.write(b'0\r\n\r\n')
    await writer.drain()


async def connection(reader, writer, judge_ms):
    try:
        while True:
            line = await reader.readline()
            if not line:
                return
            headers = {}
            while True:
                header = await reader.readline()
                if header in (b'\r\n', b'\n', b''):
                    break
                key, _, value = header.decode().partition(':')
                headers[key.strip().lower()] = value.strip()
            body = await reader.readexactly(int(headers.get('content-length', 0)))
            if not line.startswith(b'POST'):
                writer.write(b'HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n')
                continue
            await respond(json.loads(body), writer, judge_ms)
    except (ConnectionError, asyncio.IncompleteReadError, json.JSONDecodeError):
        pass
    finally:
        writer.close()


def model_main(port_file, judge_ms):
    async def main():
        server = await asyncio.start_server(
            lambda r, w: connection(r, w, judge_ms), '127.0.0.1', 0, backlog=4096)
        Path(port_file).write_text(str(server.sockets[0].getsockname()[1]))
        await server.serve_forever()
    asyncio.run(main())


# The daemon ----------------------------------------------------------------

class Conn:
    """One socket session: pipelined requests, replies matched by id."""

    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(str(path))
        self.reader = self.sock.makefile('r')
        self.replies, self.events = {}, 0
        self.cond = threading.Condition()
        self.next_id = 0
        self.ready = threading.Event()
        threading.Thread(target=self.read, daemon=True).start()
        self.ready.wait(10)

    def read(self):
        try:
            for line in self.reader:
                message = json.loads(line)
                if message.get('event') == 'ready':
                    self.ready.set()
                if 'id' in message and ('result' in message or 'error' in message):
                    with self.cond:
                        self.replies[message['id']] = message
                        self.cond.notify_all()
                else:
                    self.events += 1
        except (OSError, ValueError):
            pass

    def many(self, requests, timeout=120):
        ids, lines = [], []
        for op, params in requests:
            self.next_id += 1
            ids.append(self.next_id)
            lines.append(json.dumps({'id': self.next_id, 'op': op, **params}))
        for at in range(0, len(lines), 256):
            self.sock.sendall(('\n'.join(lines[at:at + 256]) + '\n').encode())
        deadline = time.monotonic() + timeout
        with self.cond:
            while not all(i in self.replies for i in ids):
                if not self.cond.wait(deadline - time.monotonic()):
                    raise TimeoutError(f'{sum(i not in self.replies for i in ids)} replies missing')
            return [self.replies.pop(i) for i in ids]

    def request(self, op, **params):
        return self.many([(op, params)])[0]


def cpu_seconds(pid):
    try:
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / TICK
    except (FileNotFoundError, ProcessLookupError):
        return 0.0


def rss_mib(pid):
    try:
        return int(Path(f'/proc/{pid}/statm').read_text().split()[1]) * PAGE / 2**20
    except (FileNotFoundError, ProcessLookupError):
        return 0.0


def sockets(pid):
    found = set()
    try:
        for fd in os.listdir(f'/proc/{pid}/fd'):
            try:
                link = os.readlink(f'/proc/{pid}/fd/{fd}')
            except OSError:
                continue
            if link.startswith('socket:'):
                found.add(link)
    except OSError:
        pass
    return found


class Sampler:
    """CPU seconds and peak RSS of a process over a window, and every
    socket it held: more than it started with means it connected again."""

    def __init__(self, pid):
        self.pid, self.peak, self.stop = pid, 0.0, threading.Event()
        self.start_cpu, self.start = cpu_seconds(pid), time.monotonic()
        self.first = sockets(pid)
        self.seen = set(self.first)
        threading.Thread(target=self.watch, daemon=True).start()

    def watch(self):
        while not self.stop.wait(.25):
            self.peak = max(self.peak, rss_mib(self.pid))
            self.seen |= sockets(self.pid)

    def done(self):
        self.stop.set()
        elapsed = time.monotonic() - self.start
        cpu = cpu_seconds(self.pid) - self.start_cpu
        return {'cpu_s': round(cpu, 2), 'cpu_pct': round(100 * cpu / elapsed, 1),
                'peak_rss_mib': round(max(self.peak, rss_mib(self.pid)), 1),
                'seconds': round(elapsed, 1), 'new_sockets': len(self.seen - self.first)}


class Bench:
    def __init__(self, args):
        self.args = args
        root = Path(__file__).resolve().parents[1]
        self.agent = root / '.local/target/release/agent'
        self.sonny = root / '.local/target/release/sonny'
        self.out = Path(args.out)
        self.out.mkdir(parents=True, exist_ok=True)
        self.home = self.out / 'home'
        (self.home / '.agent').mkdir(parents=True, exist_ok=True)
        self.store = self.out / 'state.sqlite'
        self.tmp = Path(tempfile.mkdtemp(prefix='ls-', dir='/tmp'))
        self.socket = self.tmp / 'd.sock'
        port_file = self.tmp / 'port'
        self.model = subprocess.Popen(
            [sys.executable, '-m', 'bench.lookout_scale', '--model', str(port_file),
             '--judge-ms', str(args.judge_ms)], cwd=root, start_new_session=True)
        while not port_file.exists() or not port_file.read_text():
            if self.model.poll() is not None:
                raise RuntimeError("model did not start")
            time.sleep(.05)
        self.url = f'http://127.0.0.1:{port_file.read_text()}/v1'
        self.env = {'PATH': os.environ['PATH'], 'HOME': str(self.home)}
        self.daemon = subprocess.Popen(
            [str(self.agent), 'serve', '--store', str(self.store), '--provider',
             f'openai=responses,{self.url}', '--socket', str(self.socket)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=open(self.out / 'daemon.err', 'w'), env=self.env, start_new_session=True)
        deadline = time.monotonic() + 10
        while True:
            try:
                self.control = Conn(self.socket)
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if time.monotonic() > deadline:
                    raise
                time.sleep(.05)
        self.lookout = None
        self.results = {'projects': args.projects, 'tasks': args.tasks,
                        'bots': args.projects * (args.tasks + 1)}

    def leads(self):
        return [f'p{p}.lead' for p in range(self.args.projects)]

    def tasks(self):
        return [f'p{p}.t{t}' for p in range(self.args.projects) for t in range(self.args.tasks)]

    def create(self, names, tools=('echo',), **extra):
        made = self.control.many([('create', {
            'bot': name, 'workspace': str(self.out), 'model': 'openai/synthetic-model',
            'instructions': 'Bench agent.', 'tools': list(tools), **extra}) for name in names])
        bad = [m for m in made if 'error' in m]
        assert not bad, bad[:3]

    def setup(self):
        started = time.monotonic()
        self.create(self.leads())
        self.create(self.tasks())
        folder = Path(str(self.store) + '.lookout')
        folder.mkdir(exist_ok=True)
        for p in range(self.args.projects):
            tasks = {'name': 'tasks', 'tasks_of': [f'p{p}.lead'], 'bots': [f'p{p}.*']}
            (folder / f'p{p}.lead.json').write_text(json.dumps({'subscriptions': [tasks]}))
        # One finished turn each lead's messages to its tasks come from: what
        # a lead writes in its tasks is no news to it, only how they end.
        firsts = self.control.many([('submit', {'bot': lead, 'request_id': 'first', 'prompt': 'hi'})
                                    for lead in self.leads()])
        self.from_turn = {lead: r['result']['turn'] for lead, r in zip(self.leads(), firsts)}
        while self.busy_tasks():
            time.sleep(.2)
        self.results['create_s'] = round(time.monotonic() - started, 2)

    def log_lines(self):
        path = Path(str(self.store) + '.lookout.jsonl')
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]

    def start_lookout(self, phase):
        log = open(self.out / f'lookout-{phase}.out', 'w')
        started = time.monotonic()
        before = len(self.log_lines())
        self.lookout = subprocess.Popen(
            [str(self.sonny), 'lookout', '--store', str(self.store), '--socket', str(self.socket)],
            stdin=subprocess.DEVNULL, stdout=log, stderr=log, env=self.env, start_new_session=True)
        sampler = Sampler(self.lookout.pid)
        # Seated: every lead has a lookout. Live: its cursor file moved, or a
        # quiet second passed with all seated.
        want = self.args.projects
        while time.monotonic() - started < 600:
            seated = sum(1 for line in self.log_lines()[before:] if line.get('seated'))
            if seated >= want:
                break
            time.sleep(.1)
        seated_s = time.monotonic() - started
        # Live once its CPU settles: a replay keeps it busy.
        last = cpu_seconds(self.lookout.pid)
        while time.monotonic() - started < 600:
            time.sleep(.5)
            now = cpu_seconds(self.lookout.pid)
            if now - last < .02:
                break
            last = now
        stats = sampler.done()
        return {'seated_s': round(seated_s, 2), 'busy_until_s': round(time.monotonic() - started - .5, 2),
                'start_cpu_s': stats['cpu_s'], 'start_peak_rss_mib': stats['peak_rss_mib']}

    def idle(self, seconds):
        sampler = Sampler(self.lookout.pid)
        daemon = Sampler(self.daemon.pid)
        time.sleep(seconds)
        return {'lookout': sampler.done(), 'daemon': daemon.done()}

    def load(self, seconds, with_lookout=True):
        """Every task runs turns back to back for `seconds`."""
        self.phase = getattr(self, 'phase', 0) + 1
        count, ms = self.args.deltas, self.args.delta_ms
        tasks = self.tasks()
        lookout = Sampler(self.lookout.pid) if with_lookout else None
        daemon = Sampler(self.daemon.pid)
        started = time.monotonic()
        submitted, n = 0, 0
        stop = started + seconds
        # Rounds: every task runs one turn, then the next round starts once
        # all have ended.
        while time.monotonic() < stop:
            batch = [('submit', {'bot': task, 'request_id': f'{self.phase}r{n}-{task}',
                                 'prompt': f'stream:{count}:{ms}',
                                 'from': {'bot': task.split('.')[0] + '.lead',
                                          'turn': self.from_turn[task.split('.')[0] + '.lead']}})
                     for task in tasks]
            n += 1
            replies = self.control.many(batch)
            submitted += sum('result' in r for r in replies)
            time.sleep(count * ms / 1000 * .8)
            while self.running_tasks() and time.monotonic() < stop + 600:
                time.sleep(.2)
        ran = time.monotonic() - started
        settle = time.monotonic()
        time.sleep(self.args.settle)
        result = {'submitted_turns': submitted, 'ran_s': round(ran, 1),
                  'daemon': daemon.done(),
                  'deltas_per_s': round(submitted * count / ran)}
        if lookout:
            result['lookout'] = lookout.done()
            result['lookout_alive'] = self.lookout.poll() is None
        return result

    def running_tasks(self):
        """Tasks with a turn running or queued, as the daemon lists them."""
        busy, after = 0, None
        while True:
            page = self.control.request('bots', limit=256, after=after)['result']
            busy += sum(1 for b in page['bots'] if '.t' in b['name']
                        and (b.get('running_turn') is not None or b.get('status') in ('running', 'queued')))
            after = page.get('next_after')
            if after is None:
                return busy

    def busy_tasks(self):
        stats = self.control.request('stats')['result']
        return stats.get('active_turns', 0) + stats.get('queued_turns', 0)

    def note_delays(self, since_ms):
        """For each task turn that ended after `since_ms`: how long until a
        lead's note naming it started."""
        tasks = self.tasks()
        ended = {}
        for task, reply in zip(tasks, self.control.many(
                [('turns', {'bot': t, 'limit': 256}) for t in tasks])):
            for turn in reply.get('result', {}).get('turns', []):
                if turn.get('finished_ms') and turn['finished_ms'] >= since_ms:
                    ended[f'turn:{task}/{turn["turn"]}'] = turn['finished_ms']
        mentions = {}
        leads = self.leads()
        notes = []
        for lead, reply in zip(leads, self.control.many(
                [('turns', {'bot': l, 'limit': 256}) for l in leads])):
            for turn in reply.get('result', {}).get('turns', []):
                if str(turn.get('request_id', '')).startswith('lookout-') and turn.get('started_ms'):
                    notes.append((lead, turn['turn'], turn['started_ms']))
        prompts = self.control.many([('prompts', {'bot': lead, 'turn': turn})
                                     for lead, turn, _ in notes])
        for (lead, turn, at), reply in zip(notes, prompts):
            for prompt in reply.get('result', {}).get('prompts', []):
                for handle in re.findall(r'turn:p\d+\.t\d+/\d+', prompt.get('text', '')):
                    mentions.setdefault(handle, []).append(at)
        # The first note after the turn ended.
        told = {h: min((at for at in mentions.get(h, []) if at >= ended[h]), default=None)
                for h in ended}
        delays = sorted(told[h] - ended[h] for h in ended if told[h] is not None)
        pct = lambda q: delays[min(len(delays) - 1, int(q * len(delays)))] if delays else None
        return {'turns_ended': len(ended), 'told': len(delays), 'notes': len(notes),
                'delay_ms_p50': pct(.5), 'delay_ms_p95': pct(.95),
                'delay_ms_max': delays[-1] if delays else None}

    def stop_lookout(self):
        if self.lookout and self.lookout.poll() is None:
            self.lookout.terminate()
            self.lookout.wait(10)

    def approver(self, gated):
        """`gated` bots each make one shell call the approver judges."""
        names = [f'g{i}' for i in range(gated)]
        self.create(names, tools=('echo', 'shell'), approve=['shell'], approver='auto')
        log = open(self.out / 'approver.out', 'w')
        process = subprocess.Popen(
            [str(self.agent), 'approver', '--store', str(self.store), '--socket', str(self.socket),
             '--judge', 'openai/synthetic-model'],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=log, env=self.env, text=True,
            start_new_session=True)
        lines = []
        threading.Thread(target=lambda: [lines.append(json.loads(l)) for l in process.stdout],
                         daemon=True).start()
        while not any(l.get('event') == 'serving' for l in lines):
            time.sleep(.05)
            if process.poll() is not None:
                raise RuntimeError('approver did not start')
        sampler = Sampler(process.pid)
        started = time.monotonic()
        self.control.many([('submit', {'bot': name, 'request_id': 'gate', 'prompt': 'shell:true'})
                           for name in names])
        decided = lambda: [c for l in lines if l.get('event') == 'judged' for c in l['calls']]
        while len(decided()) < gated and time.monotonic() - started < 300:
            time.sleep(.25)
        took = time.monotonic() - started
        calls = decided()
        stats = sampler.done()
        judge_ms = sorted(l['judge_ms'] for l in lines if l.get('event') == 'judged')
        process.terminate()
        process.wait(10)
        reasons = {}
        for call in calls:
            key = call['decision'] + (': ' + call['reason'][:40] if call.get('reason') else '')
            reasons[key] = reasons.get(key, 0) + 1
        return {'gated': gated, 'judge_ms_model': self.args.judge_ms, 'decided': len(calls),
                'seconds': round(took, 1), 'decisions': reasons,
                'round_ms_p50': judge_ms[len(judge_ms) // 2] if judge_ms else None,
                'round_ms_max': judge_ms[-1] if judge_ms else None, 'approver': stats}

    def close(self):
        self.stop_lookout()
        try:
            self.control.request('shutdown')
            self.daemon.wait(10)
        except Exception:
            self.daemon.kill()
        os.killpg(self.model.pid, 15)
        shutil.rmtree(self.tmp, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--model')
    parser.add_argument('--judge-ms', type=int, default=0)
    parser.add_argument('--projects', type=int, default=10)
    parser.add_argument('--tasks', type=int, default=10)
    parser.add_argument('--deltas', type=int, default=40)
    parser.add_argument('--delta-ms', type=int, default=100)
    parser.add_argument('--load-s', type=float, default=30)
    parser.add_argument('--settle', type=float, default=10)
    parser.add_argument('--idle-s', type=float, default=10)
    parser.add_argument('--phases', default='idle,baseline,load,restart')
    parser.add_argument('--gated', type=int, default=0)
    parser.add_argument('--out')
    args = parser.parse_args()
    if args.model:
        return model_main(args.model, args.judge_ms)
    bench = Bench(args)
    phases = args.phases.split(',')
    try:
        bench.setup()
        results = bench.results
        if 'baseline' in phases:
            results['baseline'] = bench.load(args.load_s, with_lookout=False)
        results['start'] = bench.start_lookout('start')
        if 'idle' in phases:
            results['idle'] = bench.idle(args.idle_s)
        if 'load' in phases:
            since = int(time.time() * 1000)
            results['load'] = bench.load(args.load_s)
            results['load']['notes'] = bench.note_delays(since)
        if 'restart' in phases:
            bench.stop_lookout()
            results['restart'] = bench.start_lookout('restart')
        if args.gated:
            watching = Sampler(bench.lookout.pid) if bench.lookout.poll() is None else None
            results['approver'] = bench.approver(args.gated)
            if watching:
                results['approver']['lookout'] = watching.done()
        reconnects = sum(1 for p in bench.out.glob('lookout-*.out')
                         for line in p.read_text().splitlines() if '"watching"' in line)
        results['lookout_processes_started'] = reconnects
        print(json.dumps(results, indent=1))
        (bench.out / 'results.json').write_text(json.dumps(results, indent=1))
    finally:
        bench.close()


if __name__ == '__main__':
    main()
