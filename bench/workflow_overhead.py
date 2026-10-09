"""What a workflow run costs per agent, against the synthetic model.

Two arms do the same work: N fresh bots, at most P at a time, each one
`agent run --new --detach` and one `wait` on a shared daemon connection.
`workflow` runs it as a plan through app/skills/workflow/workflow.py, with
its labels, events and summary; `direct` is a bare loop of the same
commands. Each arm runs as its own process, sampled from /proc for its own
CPU, its children's (the `agent` commands) and its peak RSS; the daemon is
sampled the same way. Linux only (/proc).

    python3 -m bench.workflow_overhead [--agents N] [--parallel P] [--delay-ms D] [--repeat R]
"""
import argparse
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

from bench import synthetic_model

ROOT = Path(__file__).resolve().parent.parent
RUNNER = ROOT / 'app/skills/workflow/workflow.py'
BINARY = ROOT / '.local/target/release/agent'
TICK = os.sysconf('SC_CLK_TCK')


def sample(pid):
    """CPU seconds (own, waited children) and peak RSS (MiB) of a live pid."""
    stat = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    own = (int(stat[11]) + int(stat[12])) / TICK
    children = (int(stat[13]) + int(stat[14])) / TICK
    peak = next(int(line.split()[1]) for line in Path(f'/proc/{pid}/status').read_text().splitlines()
                if line.startswith('VmHWM:')) / 1024
    return own, children, peak


def watch(process):
    last = (0, 0, 0)
    while process.poll() is None:
        try:
            last = sample(process.pid)
        except (OSError, StopIteration, IndexError):
            pass
        time.sleep(.01)
    return last


def direct(agents, parallel, delay, round_):
    """The baseline arm: the same commands with no runner bookkeeping."""
    ready = json.loads(subprocess.run([str(BINARY), 'start'], capture_output=True, text=True).stdout)
    conn = socket.socket(socket.AF_UNIX)
    conn.connect(ready['socket'])
    file = conn.makefile('rw')
    file.readline()
    lock, waiting, slots = threading.Lock(), {}, threading.BoundedSemaphore(parallel)

    def read():
        for line in file:
            reply = json.loads(line)
            waiting.pop(reply['id']).set()
    threading.Thread(target=read, daemon=True).start()

    def one(i):
        with slots:
            out = subprocess.run([str(BINARY), 'run', '--new', '--detach', '--no-spawn', '--instructions=x',
                                  '--bot', f'direct{round_}.w{i}', '--', '-'], input=f'delay:{delay}',
                                 capture_output=True, text=True, check=True).stdout
            done = threading.Event()
            with lock:
                waiting[i] = done
                file.write(json.dumps({'id': i, 'op': 'wait', 'handles': [json.loads(out)['handle']]}) + '\n')
                file.flush()
            done.wait()
    threads = [threading.Thread(target=one, args=(i,)) for i in range(agents)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()


def arm(name, env, workdir, agents, parallel, delay, round_):
    if name == 'workflow':
        plan = workdir / 'plan.py'
        plan.write_text(f'result = sum(r.ok for r in parallel(range({agents}), lambda i: agent('
                        f'"delay:{delay}", label=f"w{{i}}", instructions="x")))\n')
        command = [sys.executable, str(RUNNER), 'start', str(plan), '--name', f'bench{round_}',
                   '--parallel', str(parallel), '--max-agents', str(agents)]
    else:
        command = [sys.executable, '-m', 'bench.workflow_overhead', '--direct', '--agents', str(agents),
                   '--parallel', str(parallel), '--delay-ms', str(delay), '--round', str(round_)]
    env = {**env, 'HOME': str(workdir / f'home-{name}-{round_}')}
    Path(env['HOME']).mkdir()
    started = time.monotonic()
    process = subprocess.Popen(command, env=env, cwd=ROOT if name == 'direct' else workdir,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    out = []
    reader = threading.Thread(target=lambda: out.append(process.communicate()), daemon=True)
    reader.start()
    own, children, peak = watch(process)
    reader.join()
    wall = time.monotonic() - started
    if process.returncode != 0:
        raise SystemExit(f'{name} failed: {out[0][1][-2000:]}')
    return {'arm': name, 'wall_s': round(wall, 3), 'own_cpu_s': round(own, 3), 'agent_cli_cpu_s': round(children, 3),
            'peak_rss_mib': round(peak, 1)}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--agents', type=int, default=200)
    parser.add_argument('--parallel', type=int, default=64)
    parser.add_argument('--delay-ms', type=int, default=500)
    parser.add_argument('--repeat', type=int, default=2)
    parser.add_argument('--direct', action='store_true')
    parser.add_argument('--round', type=int, default=0)
    args = parser.parse_args()
    if args.direct:
        return direct(args.agents, args.parallel, args.delay_ms, args.round)
    server, url = synthetic_model.start()
    with tempfile.TemporaryDirectory() as temp:
        workdir = Path(temp)
        env = {'PATH': os.environ['PATH'], 'AGENT_BIN': str(BINARY), 'AGENT_MODEL': 'stub/model',
               'AGENT_STORE': str(workdir / 'state.sqlite'), 'AGENT_SOCKET': str(workdir / 'agent.sock')}
        daemon = subprocess.Popen([str(BINARY), 'serve', '--store', env['AGENT_STORE'], '--socket', env['AGENT_SOCKET'],
                                   '--provider', f'stub=responses,{url}'], env=env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            while not Path(env['AGENT_SOCKET']).exists():
                time.sleep(.02)
            rows = []
            for round_ in range(args.repeat):
                # Alternate the order, so neither arm always meets a fuller store.
                for name in (('workflow', 'direct') if round_ % 2 == 0 else ('direct', 'workflow')):
                    before = sample(daemon.pid)[0]
                    row = arm(name, env, workdir, args.agents, args.parallel, args.delay_ms, round_)
                    row['daemon_cpu_s'] = round(sample(daemon.pid)[0] - before, 3)
                    row['round'] = round_
                    rows.append(row)
                    print(json.dumps(row), flush=True)
            ideal = -(-args.agents // args.parallel) * args.delay_ms / 1000
            print(json.dumps({'agents': args.agents, 'parallel': args.parallel, 'delay_ms': args.delay_ms,
                              'ideal_wall_s': ideal, 'daemon_peak_rss_mib': round(sample(daemon.pid)[2], 1)}))
        finally:
            subprocess.run([str(BINARY), 'shutdown'], env=env, capture_output=True, timeout=60)
            daemon.wait(60)
            server.shutdown()


if __name__ == '__main__':
    main()
