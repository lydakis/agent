"""Sustained live load: N bots take turns back to back for M minutes through one daemon.

A paid check against a real provider, never part of the test suite. Each bot
resubmits as soon as its previous turn finishes, whatever the outcome, so the
provider sees a steady stream rather than one burst. The driver records
throughput, latency, and failures by error code per window, daemon RSS,
threads, and open files over time, and store growth. Context is bounded with
--context-items so requests stay the same size as histories grow. The key
comes from the caller's environment and is never printed or stored.

    .local/venv/bin/python -m bench.sustained --bots 64 --minutes 5 \
        --model openai/gpt-5.6-luna --out .local/bench/sustained-luna-64
"""
import argparse
import json
import os
import queue
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import psutil

from .runtime_client import Client
from .targets import file_hash

PROMPT = 'Run wc -l notes.txt with the shell tool and report just the number.'
ENDPOINTS = {'openai': ('responses', 'https://api.openai.com/v1', 'OPENAI_API_KEY'),
             'anthropic': ('anthropic', 'https://api.anthropic.com/v1', 'ANTHROPIC_API_KEY')}


def percentile(values, p):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, int(len(ordered) * p) - 1))]


def run(agent, out, bots, minutes, model, context_items, window_seconds, retain_turns=None):
    store = out / 'state.sqlite'
    workspace = out / 'workspace'
    workspace.mkdir(parents=True)
    (workspace / 'notes.txt').write_text('alpha\nbeta\ngamma\n')
    provider, name = model.split('/', 1)
    family, url, key_env = ENDPOINTS[provider]
    if key_env not in os.environ:
        raise SystemExit(f'{key_env} is not set')
    client = Client(agent, store, url, 'shell,read,write,edit,wait', model=name, key_env=key_env,
                    env=os.environ.copy(), provider=provider, family=family,
                    extra=('--context-items', str(context_items),
                           *(['--retain-turns', str(retain_turns)] if retain_turns else [])))
    daemon = psutil.Process(client.process.pid)
    for index in range(bots):
        client.request('create', bot=f'b{index}', workspace=str(workspace), reasoning='low')
    started = time.monotonic()
    deadline = started + minutes * 60
    submitted_at, sequence = {}, {}
    windows, samples = [], []
    current = None

    def new_window():
        return dict(start_s=round(time.monotonic() - started, 1), completed=0, failed={}, latency_ms=[])

    def submit(bot):
        sequence[bot] = sequence.get(bot, 0) + 1
        submitted_at[bot] = time.monotonic()
        client.request('submit', bot=bot, request_id=f's{sequence[bot]}', prompt=PROMPT)

    def sample():
        try:
            samples.append(dict(t_s=round(time.monotonic() - started, 1), rss_mib=round(daemon.memory_info().rss / 2**20, 2),
                                threads=daemon.num_threads(), fds=daemon.num_fds(),
                                store_mib=round(sum(p.stat().st_size for p in out.glob('state.sqlite*')) / 2**20, 2)))
        except psutil.Error:
            pass

    def handle(message):
        nonlocal current
        if message.get('event') != 'turn_finished':
            return
        bot = message['bot']
        data = message['data']
        latency = (time.monotonic() - submitted_at.pop(bot)) * 1000
        if data['status'] == 'completed':
            current['completed'] += 1
            current['latency_ms'].append(latency)
        else:
            code = data.get('error') or data['status']
            current['failed'][code] = current['failed'].get(code, 0) + 1
        if time.monotonic() < deadline:
            submit(bot)

    current = new_window()
    sample()
    for index in range(bots):
        submit(f'b{index}')
    next_window = started + window_seconds
    next_sample = started + 10
    while submitted_at:
        for message in client.saved:
            handle(message)
        client.saved.clear()
        try:
            handle(client.queue.get(timeout=.2))
        except queue.Empty:
            pass
        now = time.monotonic()
        if now >= next_window:
            windows.append(current)
            current = new_window()
            next_window += window_seconds
        if now >= next_sample:
            sample()
            next_sample += 10
    windows.append(current)
    sample()
    elapsed = time.monotonic() - started
    totals = dict(completed=0, failed={}, input_tokens=0, output_tokens=0, turns=0)
    for index in range(bots):
        after = 0
        while True:
            page = client.request('turns', bot=f'b{index}', after=after, limit=256)['result']
            for turn in page['turns']:
                totals['turns'] += 1
                totals['input_tokens'] += turn['input_tokens']
                totals['output_tokens'] += turn['output_tokens']
                if turn['status'] == 'completed':
                    totals['completed'] += 1
                else:
                    totals['failed'][turn['status']] = totals['failed'].get(turn['status'], 0) + 1
            if not page['turns'] or page.get('next_after') is None:
                break
            after = page['next_after']
    client.request('shutdown')
    client.close()
    for window in windows:
        latency = window.pop('latency_ms')
        window.update(p50_ms=round(percentile(latency, .5) or 0), p95_ms=round(percentile(latency, .95) or 0),
                      max_ms=round(max(latency) if latency else 0), turns_per_s=round(window['completed'] / window_seconds, 2))
    failed_by_code = {}
    for window in windows:
        for code, count in window['failed'].items():
            failed_by_code[code] = failed_by_code.get(code, 0) + count
    return dict(schema='sustained_v1', created_at=datetime.now(timezone.utc).isoformat(), model=model, bots=bots,
                minutes=minutes, context_items=context_items, retain_turns=retain_turns, prompt=PROMPT,
                binary_sha256=file_hash(agent),
                elapsed_seconds=round(elapsed, 1), window_seconds=window_seconds, windows=windows,
                failed_by_code=failed_by_code, totals=totals, samples=samples,
                host=dict(system=os.uname().sysname, machine=os.uname().machine))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--bots', type=int, default=64)
    parser.add_argument('--minutes', type=float, default=5)
    parser.add_argument('--model', required=True)
    parser.add_argument('--context-items', type=int, default=8)
    parser.add_argument('--window-seconds', type=int, default=30)
    parser.add_argument('--retain-turns', type=int, default=None, help='daemon retention policy; none by default')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    out = args.out.resolve()
    if Path.cwd() != root or not out.is_relative_to(root / '.local') or out.exists():
        parser.error('run from the repository root with a new output directory under .local')
    if not (1 <= args.bots <= 1024) or not (0.1 <= args.minutes <= 60):
        parser.error('--bots must be 1 to 1024 and --minutes 0.1 to 60')
    result = run(args.binary.resolve(), out, args.bots, args.minutes, args.model, args.context_items,
                 args.window_seconds, args.retain_turns)
    (out / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps({k: v for k, v in result.items() if k not in ('windows', 'samples')}))
    return 0 if not result['failed_by_code'] else 1


if __name__ == '__main__':
    sys.exit(main())
