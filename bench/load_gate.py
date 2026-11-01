"""How the daemon's load gate (`--max-load`, #575) runs a busy machine.

One socket daemon on the synthetic model of `bench.lookout_scale`. W
workers each run R turns back to back, and each turn runs one shell command
that spends a fixed amount of CPU. Meanwhile a probe measures what the
person feels every half second:

- a turn that runs no command (submit to its end);
- a short command (`true`), which the gate holds like any other;
- a control request (`bots`).

Run once with no gate and once with one. Compare the work's wall time (what
the gate costs), how many work commands ran at once, and the 1-minute load
it reached, against the probe's latencies (what the gate buys). The
daemon's CPU is split by thread, to say where its own work went.

Run from the repo root after `cargo build --release --workspace --bins`:
    python3 -m bench.load_gate --max-load 0 --max-load 8 --out /tmp/x
`--max-load 0` is no gate.
"""
import argparse
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path
from types import SimpleNamespace

from bench.lookout_scale import TICK, Bench, Conn, Sampler

MARK = 'gatebusy'
# A work command has as long as the shell tool allows: a run counts only work
# that finished, so none may end on its timeout.
TIMEOUT_MS = 600_000


def busy_command(iterations, done):
    """Each command adds a line to `done` once its work is finished, so a run
    counts only work that ran to the end, not a command its timeout ended.
    The mark names the process, so the sampler can count work commands."""
    return (f"python3 -c \"import sys; sum(range({iterations})); "
            f"open(sys.argv[1], 'a').write('.')\" {done} {MARK}")


def calibrate():
    """Iterations of `sum(range(n))` that take about a second here."""
    n = 2_000_000
    while True:
        started = time.monotonic()
        subprocess.run([sys.executable, '-c', f'sum(range({n}))'], check=True)
        took = time.monotonic() - started
        if took > .25:
            return int(n / took)
        n *= 4


def running_work():
    count = 0
    for pid in os.listdir('/proc'):
        if not pid.isdigit():
            continue
        try:
            line = Path(f'/proc/{pid}/cmdline').read_bytes()
        except OSError:
            continue
        if MARK.encode() in line and b'python3' in line.split(b'\0')[0]:
            count += 1
    return count


def turn_ended(conn, bot, turn, timeout=600):
    """Waits on the connection's own session, which waits for nothing else,
    so the screen adds no polling of its own to the daemon's work."""
    handle, deadline = f'turn:{bot}/{turn}', time.monotonic() + timeout
    while time.monotonic() < deadline:
        found = conn.request('wait', handles=[handle], timeout_ms=30_000)['result']['results'][handle]
        if not found.get('pending'):
            return True
    return False


def threads_cpu(pid):
    """CPU seconds of each of a process's threads, by thread name."""
    found = {}
    for tid in os.listdir(f'/proc/{pid}/task'):
        try:
            text = Path(f'/proc/{pid}/task/{tid}/stat').read_text()
        except OSError:
            continue
        name = text[text.index('(') + 1:text.rindex(')')]
        fields = text.rsplit(')', 1)[1].split()
        found[name] = found.get(name, 0) + (int(fields[11]) + int(fields[12])) / TICK
    return found


def pct(values, q):
    if not values:
        return None
    values = sorted(values)
    return round(values[min(len(values) - 1, int(q * len(values)))], 1)


def run(args, gate):
    # Each run starts from a quiet machine: the 1-minute load carries over.
    deadline = time.monotonic() + 600
    while os.getloadavg()[0] >= args.quiet and time.monotonic() < deadline:
        time.sleep(5)
    out = Path(args.out) / (f'gate-{gate:g}' if gate else 'no-gate')
    flags = ['--max-processes', str(args.processes), '--max-load', str(gate)]
    bench = Bench(SimpleNamespace(agent=args.agent, sonny=args.sonny, projects=0, tasks=0,
                                  judge_ms=0, out=str(out), daemon_flags=flags))
    workers = [f'w{i}' for i in range(args.workers)]
    bench.create(workers + ['probe', 'probe.sh'], tools=('shell',))
    done = out / 'done'
    command = busy_command(args.iterations, done)
    stop, samples = threading.Event(), {'work': [], 'load': []}

    def sample():
        while not stop.wait(.25):
            samples['work'].append(running_work())
            samples['load'].append(os.getloadavg()[0])

    probes = {'turn_ms': [], 'short_command_ms': [], 'control_ms': []}

    # Each probe samples on its own connection and thread, so a `true` the
    # gate holds does not stop the other two from sampling while it is shut.
    def control():
        conn = Conn(bench.socket)
        while not stop.wait(.5):
            started = time.monotonic()
            conn.request('bots', limit=1)
            probes['control_ms'].append(1000 * (time.monotonic() - started))

    def turns(bot, prompt, key):
        conn, n = Conn(bench.socket), 0
        while not stop.wait(.5):
            n += 1
            started = time.monotonic()
            reply = conn.request('submit', bot=bot, request_id=f'p{n}', prompt=prompt,
                                 delivery='reject')
            if 'result' in reply and turn_ended(conn, bot, reply['result']['turn'], 120):
                probes[key].append(1000 * (time.monotonic() - started))

    def worker(bot):
        conn = Conn(bench.socket)
        for r in range(args.rounds):
            reply = conn.request('submit', bot=bot, request_id=f'r{r}',
                                 prompt=f'shell@{TIMEOUT_MS}:{command}', delivery='reject')
            turn_ended(conn, bot, reply['result']['turn'])

    daemon = Sampler(bench.daemon.pid)
    before = threads_cpu(bench.daemon.pid)
    watching = [threading.Thread(target=sample, daemon=True),
                threading.Thread(target=control, daemon=True),
                threading.Thread(target=turns, args=('probe', 'hi', 'turn_ms'), daemon=True),
                threading.Thread(target=turns, args=('probe.sh', 'shell:true', 'short_command_ms'),
                                 daemon=True)]
    for thread in watching:
        thread.start()
    started = time.monotonic()
    working = [threading.Thread(target=worker, args=(w,)) for w in workers]
    for thread in working:
        thread.start()
    for thread in working:
        thread.join()
    wall = time.monotonic() - started
    stop.set()
    for thread in watching:
        thread.join(130)
    stats = daemon.done()
    after = threads_cpu(bench.daemon.pid)
    spent = sorted(((round(cpu - before.get(name, 0), 2), name) for name, cpu in after.items()),
                   reverse=True)
    bench.close()
    finished = len(done.read_text()) if done.exists() else 0
    if finished != args.workers * args.rounds:
        sys.exit(f'{finished} of {args.workers * args.rounds} work commands finished their work '
                 f'with gate {gate:g}: the run does not compare')
    return {
        'gate': gate or None, 'workers': args.workers, 'rounds': args.rounds,
        'work_wall_s': round(wall, 1),
        'work_running_max': max(samples['work'], default=0),
        'work_running_mean': round(sum(samples['work']) / max(1, len(samples['work'])), 1),
        'load_1m_max': round(max(samples['load'], default=0), 2),
        'probe_turn_ms_p50': pct(probes['turn_ms'], .5), 'probe_turn_ms_p95': pct(probes['turn_ms'], .95),
        'short_command_ms_p50': pct(probes['short_command_ms'], .5),
        'short_command_ms_p95': pct(probes['short_command_ms'], .95),
        'control_ms_p50': pct(probes['control_ms'], .5), 'control_ms_p95': pct(probes['control_ms'], .95),
        'probes': {key: len(found) for key, found in probes.items()},
        'daemon_cpu_s': stats['cpu_s'], 'daemon_peak_rss_mib': stats['peak_rss_mib'],
        'daemon_threads_cpu_s': {name: cpu for cpu, name in spent[:4]},
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--agent')
    parser.add_argument('--sonny')
    cpus = os.cpu_count() or 1
    parser.add_argument('--workers', type=int, default=4 * cpus)
    parser.add_argument('--rounds', type=int, default=10)
    parser.add_argument('--processes', type=int, default=256)
    parser.add_argument('--max-load', type=float, action='append',
                        help='each a run; 0 is no gate (default: 0, then cpus * 2)')
    parser.add_argument('--iterations', type=int, help='work per command (default: about a CPU second)')
    parser.add_argument('--quiet', type=float, default=1.0,
                        help='each run waits for the 1-minute load to fall under this')
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    args.iterations = args.iterations or calibrate()
    gates = args.max_load or [0, cpus * 2]
    results = {'cpus': cpus, 'iterations': args.iterations,
               'runs': [run(args, gate) for gate in gates]}
    print(json.dumps(results, indent=1))


if __name__ == '__main__':
    main()
