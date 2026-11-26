"""How fast a written follow takes effect, and what keeping up with the
follows folder costs the lookout, at N subscribers.

One socket daemon on the synthetic model (as `bench.lookout_scale`), N
leads, each with a follow file in `STORE.lookout/` as Sonny writes one
(`subs::Sub::lead`), and `sonny lookout` watching them all. Every write
here is a whole file renamed into place, as every Sonny writer does, and
none is a `follows` or `commons` call: these are the writes start_task,
Sonny and a person make. Phases:

- idle: the lookout's CPU over `--idle-s` with nothing written;
- singles: one follow rewritten every second, `--singles` times; how long
  until each is live, from the lookout's own `reloaded` line;
- bursts: `--bursts` follows rewritten at once, each burst in turn; how long
  until all are live, and the lookout's CPU over a fixed window from the
  burst's start (`--window-s`), so a build that reads on a timer and one
  that reads on each write face the same span.

Run from the repo root after building `agent` and `sonny` in release:
    python3 -m bench.follows_watch --agents 5000 --sonny PATH --out DIR
"""
import argparse
import json
import os
import time
from pathlib import Path

import subprocess

from bench import lookout_scale
from bench.lookout_scale import Bench, Sampler


def cpu_seconds(pid):
    """A process's CPU seconds: from /proc on Linux, from `ps` on macOS."""
    if Path('/proc').is_dir():
        return proc_cpu_seconds(pid)
    try:
        text = subprocess.run(['ps', '-o', 'time=', '-p', str(pid)], capture_output=True,
                              text=True, check=True).stdout.strip()
    except (subprocess.CalledProcessError, OSError):
        return 0.0
    # [[dd-]hh:]mm:ss.ss
    days, _, clock = text.rpartition('-')
    seconds = 0.0
    for part in clock.split(':'):
        seconds = seconds * 60 + float(part)
    return seconds + (int(days) * 86400 if days else 0)


proc_cpu_seconds = lookout_scale.cpu_seconds
# The samplers count CPU this way on either system.
lookout_scale.cpu_seconds = cpu_seconds


class Follows(Bench):
    def leads(self):
        return [f'p{p}.lead' for p in range(self.args.agents)]

    def tasks(self):
        return []

    def folder(self):
        return Path(str(self.store) + '.lookout')

    def follow(self, lead, filtered):
        project = lead.removesuffix('.lead')
        tasks = {'name': 'tasks', 'tasks_of': [lead], 'agents': [f'{project}.*']}
        if filtered:
            tasks['filter'] = {'type': ['turn.ended']}
        return {'subscriptions': [tasks]}

    def write(self, lead, filtered):
        """As Sonny writes a follow: whole, then renamed into place."""
        path = self.folder() / f'{lead}.json'
        partial = self.folder() / f'{lead}.json.{os.getpid()}.partial'
        partial.write_text(json.dumps(self.follow(lead, filtered), indent=1) + '\n')
        os.replace(partial, path)

    def setup(self):
        started = time.monotonic()
        names = self.leads()
        for at in range(0, len(names), 1000):
            self.create(names[at:at + 1000])
        self.folder().mkdir(exist_ok=True)
        for lead in names:
            self.write(lead, False)
        self.filtered = {lead: False for lead in names}
        self.results['create_s'] = round(time.monotonic() - started, 2)

    def start(self):
        log = open(self.out / 'lookout.out', 'w')
        started = time.monotonic()
        self.lookout = subprocess.Popen(
            [str(self.sonny), 'lookout', '--store', str(self.store), '--socket', str(self.socket)],
            stdin=subprocess.DEVNULL, stdout=log, stderr=log, env=self.env, start_new_session=True)
        self.lines_seen = 0
        while time.monotonic() - started < 900:
            if self.seated() >= self.args.agents:
                break
            time.sleep(.2)
        seated_s = time.monotonic() - started
        # Settled once its CPU does: a replay or a first read keeps it busy.
        last = cpu_seconds(self.lookout.pid)
        while time.monotonic() - started < 900:
            time.sleep(1)
            now = cpu_seconds(self.lookout.pid)
            if now - last < .02:
                break
            last = now
        return {'seated_s': round(seated_s, 2), 'settled_s': round(time.monotonic() - started, 2)}

    def seated(self):
        return sum(1 for line in self.log_lines() if line.get('seated'))

    def reloaded(self, since_ms):
        """When each lead's file was read changed, from `since_ms` on."""
        found = {}
        for line in self.log_lines():
            if line.get('reloaded') and line.get('at_ms', 0) >= since_ms:
                found.setdefault(line['lead'], line['at_ms'])
        return found

    def flip(self, leads):
        """Rewrite each lead's follow, its filter toggled: when each landed."""
        at = {}
        for lead in leads:
            self.filtered[lead] = not self.filtered[lead]
            self.write(lead, self.filtered[lead])
            at[lead] = time.time() * 1000
        return at

    def singles(self, count, first):
        leads = self.leads()[first:first + count]
        since = int(time.time() * 1000)
        written = {}
        for lead in leads:
            written.update(self.flip([lead]))
            time.sleep(1)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            live = self.reloaded(since)
            if all(lead in live for lead in leads):
                break
            time.sleep(.1)
        live = self.reloaded(since)
        waits = sorted(live[lead] - written[lead] for lead in leads if lead in live)
        return summary(waits, count)

    def burst(self, size, first, window_s):
        leads = self.leads()[first:first + size]
        since = int(time.time() * 1000)
        sampler = Sampler(self.lookout.pid)
        started = time.monotonic()
        written = self.flip(leads)
        write_s = time.monotonic() - started
        all_live_s = None
        while time.monotonic() - started < window_s:
            live = self.reloaded(since)
            if all_live_s is None and all(lead in live for lead in leads):
                all_live_s = time.monotonic() - started
            time.sleep(.05)
        stats = sampler.done()
        live = self.reloaded(since)
        waits = sorted(live[lead] - written[lead] for lead in leads if lead in live)
        result = summary(waits, size)
        result.update({'writes_s': round(write_s, 3),
                       'all_live_s': round(all_live_s, 3) if all_live_s is not None else None,
                       'cpu_s_window': stats['cpu_s'], 'window_s': window_s})
        return result


def memory(pid):
    """The lookout's own memory (anonymous) and the binary's pages it
    has in (file), which depend on which binary file was written last."""
    if not Path('/proc').is_dir():
        # macOS tells resident size whole.
        rss = subprocess.run(['ps', '-o', 'rss=', '-p', str(pid)], capture_output=True,
                             text=True).stdout.strip()
        return {'rss_mib': round(int(rss or 0) / 1024, 1)}
    status = Path(f'/proc/{pid}/status').read_text()
    kib = {line.split(':')[0]: int(line.split()[1]) for line in status.splitlines()
           if line.startswith(('RssAnon', 'RssFile'))}
    return {'anon_mib': round(kib.get('RssAnon', 0) / 1024, 1),
            'file_mib': round(kib.get('RssFile', 0) / 1024, 1)}


def summary(waits, count):
    def pct(q):
        return round(waits[min(len(waits) - 1, int(q * len(waits)))]) if waits else None
    return {'written': count, 'live': len(waits), 'live_ms_p50': pct(.5),
            'live_ms_p95': pct(.95), 'live_ms_max': round(waits[-1]) if waits else None}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--agents', type=int, default=50)
    parser.add_argument('--idle-s', type=float, default=60)
    parser.add_argument('--singles', type=int, default=15)
    parser.add_argument('--bursts', default='32,100,200')
    parser.add_argument('--window-s', type=float, default=16)
    parser.add_argument('--sonny')
    parser.add_argument('--agent')
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    # What `Bench` reads of the lookout bench's arguments.
    args.judge_ms, args.projects, args.tasks = 0, args.agents, 0
    bench = Follows(args)
    try:
        bench.setup()
        results = bench.results
        results['agents'] = args.agents
        results['start'] = bench.start()
        sampler = Sampler(bench.lookout.pid)
        time.sleep(args.idle_s)
        idle = sampler.done()
        results['idle'] = {**idle, 'cpu_s_per_min': round(idle['cpu_s'] * 60 / idle['seconds'], 3),
                           **memory(bench.lookout.pid)}
        results['singles'] = bench.singles(args.singles, 0)
        first = args.singles
        results['bursts'] = {}
        for size in [int(s) for s in args.bursts.split(',')]:
            size = min(size, args.agents)
            first = first if first + size <= args.agents else 0
            results['bursts'][str(size)] = bench.burst(size, first, args.window_s)
            first += size
        results['lookout_alive'] = bench.lookout.poll() is None
        results['lookout_said'] = [json.loads(line) for line in
                                   (bench.out / 'lookout.out').read_text().splitlines()
                                   if line.startswith('{') and 'follows_' in line]
        print(json.dumps(results, indent=1))
        (bench.out / 'results.json').write_text(json.dumps(results, indent=1))
    finally:
        bench.close()


if __name__ == '__main__':
    main()
