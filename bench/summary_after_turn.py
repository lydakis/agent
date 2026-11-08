"""The wait a summary puts on the turn after it, and what summarizing as turns
end costs a fleet of idle leads.

One daemon on a synthetic Responses provider whose summaries take
`--summary-ms`, as a long one on a real provider takes its time (the Mac
measured 168 s). Nothing is spent.

`lead`: one bot runs `--turns` turns, each adding a short prompt and a
`--reply-bytes` reply to a `--context-bytes` view compacted at half, with
`--gap-ms` between a turn's end and the next prompt (the person reading and
answering). For each turn: the time from its submit to its first ordinary
request reaching the provider (the felt wait), and to its end.

`fleet`: `--bots` bots each brought just under the threshold, then one turn
each whose reply crosses it, all at once. Reported: the time until every
turn ended and every summary landed, daemon CPU and RSS before and after,
and the slowest `stats` answer meanwhile; then each bot's next turn, all at
once, and the CPU and RSS through it, where every summary is written on
either build.

    .local/venv/bin/python -m bench.summary_after_turn --binary .local/target/release/agent lead
    .local/venv/bin/python -m bench.summary_after_turn --binary .local/target/release/agent fleet --bots 200
"""
import argparse
import json
import statistics
import tempfile
import time
from pathlib import Path

import psutil

from .runtime_client import Client
from .synthetic_model import start

def daemon(binary, directory, url, context_bytes=65536):
    client = Client(binary, Path(directory) / 'state.sqlite', url,
                    settings={'context_bytes': context_bytes, 'compact_at': 50},
                    extra=('--max-active', '512'))
    return client, psutil.Process(client.process.pid)


def create(client, bot, workspace):
    response = client.request('create', bot=bot, workspace=workspace, compaction_instructions='Summarize.')
    assert 'result' in response, response


def submit(client, bot, request, prompt):
    response = client.request('submit', bot=bot, request_id=str(request), prompt=prompt)
    assert 'result' in response, response
    return response['result']['turn']


def lead(binary, turns, summary_ms, context_bytes, reply_bytes, gap_ms):
    server, url = start()
    server.summary_delay, server.arrivals = summary_ms / 1000, []
    with tempfile.TemporaryDirectory(prefix='sat-', dir='/tmp') as directory:
        client, _ = daemon(binary, directory, url, context_bytes)
        try:
            create(client, 'Lead', directory)
            waits, ends = [], []
            for n in range(turns):
                time.sleep(gap_ms / 1000 if n else 0)
                began = time.monotonic()
                turn = submit(client, 'Lead', n, f'text:{reply_bytes} {n} ' + 'p' * 400)
                ended = client.finished(turn, timeout=summary_ms / 1000 + 60)
                assert ended['data']['status'] == 'completed', ended
                first = min(at for at, summary, user in list(server.arrivals)
                            if at >= began and not summary and user.startswith(f'text:{reply_bytes} {n} '))
                waits.append((first - began) * 1000)
                ends.append((ended['_received_at'] - began) * 1000)
            time.sleep(summary_ms / 1000 + .5)
            summaries = sum(1 for _, summary, _ in server.arrivals if summary)
        finally:
            client.close()
    held = [w for w in waits if w >= 100]
    return {'turns': turns, 'summaries': summaries, 'summary_ms': summary_ms, 'gap_ms': gap_ms,
            'context_bytes': context_bytes,
            'turns_held': len(held), 'held_ms_total': round(sum(held)),
            'wait_ms_p50': round(statistics.median(waits), 1), 'wait_ms_max': round(max(waits), 1),
            'turn_ms_p50': round(statistics.median(ends), 1), 'turn_ms_max': round(max(ends), 1)}


def fleet(binary, bots, summary_ms):
    server, url = start()
    server.summary_delay, server.arrivals = summary_ms / 1000, []
    names = [f'b{n}' for n in range(bots)]
    with tempfile.TemporaryDirectory(prefix='sat-', dir='/tmp') as directory:
        client, process = daemon(binary, directory, url, 16384)
        try:
            for name in names:
                create(client, name, directory)
            # Three turns each: about 6 KiB, under the 8 KiB threshold.
            for n in range(3):
                turns = [submit(client, name, n, f'text:1500 {n} ' + 'p' * 400) for name in names]
                for turn in turns:
                    client.finished(turn, timeout=60)
            time.sleep(.5)
            rss_before, cpu_before = process.memory_info().rss, sum(process.cpu_times()[:2])
            began = time.monotonic()
            # This turn's reply carries each view past the threshold.
            turns = [submit(client, name, 3, 'text:3000 3') for name in names]
            probes = []
            for turn in turns:
                client.finished(turn, timeout=60)
            finished = time.monotonic() - began
            # Then every summary, where the daemon writes them as turns end.
            deadline = time.monotonic() + summary_ms / 1000 + 60
            landed = 0
            while time.monotonic() < deadline and landed < bots:
                asked = time.monotonic()
                client.request('stats')
                probes.append(time.monotonic() - asked)
                landed = sum(1 for m in client.saved if m.get('event') in ('compacted', 'compaction_failed'))
                if not any(summary for _, summary, _ in server.arrivals) and time.monotonic() - began > 3:
                    break  # none started: summaries wait for each bot's next turn
                time.sleep(.05)
            settled = time.monotonic() - began
            time.sleep(.5)
            rss_after, cpu_after = process.memory_info().rss, sum(process.cpu_times()[:2])
            summaries = sum(1 for _, summary, _ in server.arrivals if summary)
            # Then each bot's next turn, all at once: where a summary went
            # in as the last turn ended, none holds it.
            began = time.monotonic()
            turns = [submit(client, name, 4, 'text:10 4') for name in names]
            for turn in turns:
                client.finished(turn, timeout=summary_ms / 1000 + 60)
            next_s = time.monotonic() - began
            time.sleep(.5)
            rss_next, cpu_next = process.memory_info().rss, sum(process.cpu_times()[:2])
            next_summaries = sum(1 for _, summary, _ in server.arrivals if summary) - summaries
        finally:
            client.close()
    return {'bots': bots, 'summary_ms': summary_ms, 'summaries': summaries, 'landed': landed,
            'turns_ended_s': round(finished, 3), 'settled_s': round(settled, 3),
            'cpu_s': round(cpu_after - cpu_before, 3),
            'rss_mib_before': round(rss_before / 2**20, 1), 'rss_mib_after': round(rss_after / 2**20, 1),
            'next_turns_s': round(next_s, 3), 'next_summaries': next_summaries,
            'cpu_s_through_next': round(cpu_next - cpu_before, 3), 'rss_mib_after_next': round(rss_next / 2**20, 1),
            'stats_ms_max': round(max(probes, default=0) * 1000, 1)}


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--summary-ms', type=int, default=2000)
    commands = parser.add_subparsers(dest='command', required=True)
    one = commands.add_parser('lead')
    one.add_argument('--turns', type=int, default=24)
    one.add_argument('--context-bytes', type=int, default=65536)
    one.add_argument('--reply-bytes', type=int, default=6000)
    one.add_argument('--gap-ms', type=int, default=0)
    commands.add_parser('fleet').add_argument('--bots', type=int, default=100)
    args = parser.parse_args()
    if args.command == 'lead':
        print(json.dumps(lead(args.binary, args.turns, args.summary_ms, args.context_bytes,
                              args.reply_bytes, args.gap_ms)))
    else:
        print(json.dumps(fleet(args.binary, args.bots, args.summary_ms)))


if __name__ == '__main__':
    main()
