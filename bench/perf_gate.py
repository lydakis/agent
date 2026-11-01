"""Compare a pull request's daemon with its base's, as the Perf workflow runs them.

Reads `NAME-SIDE-ROUND/result.json` under one folder, where NAME is a bench
(admission, completion, footprint, lifecycle, sonny), SIDE is `base` or `head`,
and ROUND counts alternating runs. Store work per turn is counted, not
timed, so it is the same on every run: more of it on the head fails. CPU,
latency and memory move with the runner, so they only warn, and only when
the head's median is 15% past the base's and its best run is worse than
the base's worst. Prints a Markdown table; with
GITHUB_STEP_SUMMARY set, also writes it there.

Usage: python -m bench.perf_gate DIR
"""
import json
import os
import statistics
import sys
from pathlib import Path

# A median this far past the base's is worth a look.
WARN = 0.15
# Store work whose count depends on timing: how jobs fell into group
# commits, and how often the scheduler looked for ready turns.
TIMED = {'commit', 'next_ready'}


def measured(path):
    return [run for run in json.loads(path.read_text())['runs'] if not run.get('warmup')]


def runs(root, name, side):
    found = []
    for path in sorted(root.glob(f'{name}-{side}-*/result.json')):
        found += measured(path)
    return found


def counts(name, run):
    """What a run did in the store, by kind; the same on every run."""
    if name == 'admission':
        return {f'{phase} {kind}': value
                for phase in ('lone_submit', 'burst_create', 'burst_submit')
                for kind, value in [('jobs', run[phase]['groups']['jobs']),
                                    *((op, n) for op, n in run[phase]['operations'].items() if op not in TIMED)]}
    if name == 'completion':
        return {op: value['count'] for op, value in run['operations'].items() if op not in TIMED}
    if name == 'footprint':
        return {op: value['count'] for op, value in run['operations'].items() if op not in TIMED}
    return {}


# What each bench's runs say that moves with the runner: (label, value).
TIMINGS = {
    'admission': [('lone submit CPU, ms', lambda r: r['lone_submit']['cpu_ms']),
                  ('burst submit p99, ms', lambda r: r['burst_submit']['reply_ms']['p99'])],
    # Its CPU is read in 10 ms ticks, too coarse for one burst.
    'completion': [('release to terminal p99, ms', lambda r: r['release_to_terminal_ms']['p99'])],
    'footprint': [('idle RSS, MiB', lambda r: r['idle_rss_mib']),
                  ('per idle bot, KiB', lambda r: r['per_idle_bot_kib']),
                  ('RSS after a turn each, MiB', lambda r: r['turns_rss_mib']),
                  ('CPU per turn, ms', lambda r: r['cpu_ms_per_turn'])],
    # bench.sonny_screen: Sonny's own paths that grow with the fleet.
    'sonny': [*((f'lookout CPU per 1k deltas at {n}, ms', lambda r, n=n: r['lookout'][n]['cpu_ms_per_1k_deltas'])
                for n in ('10', '100', '1000')),
              ('lookout seated at 1000, s', lambda r: r['lookout']['1000']['start']['seated_s']),
              ('lookout note p95 at 1000, ms', lambda r: r['lookout']['1000']['note_delay_ms_p95']),
              ('quiet window CPU, ms/s', lambda r: r['window']['cpu_ms_per_s']),
              ('quiet window reads, KiB/s', lambda r: r['window']['read_kib_per_s']),
              ('quiet window requests, /s', lambda r: r['window']['requests_per_s']),
              ('quiet window RSS, MiB', lambda r: r['window']['peak_rss_mib']),
              ('sky replay CPU, s', lambda r: r['sky']['cpu_s']),
              ('sky replay RSS, MiB', lambda r: r['sky']['peak_rss_mib'])],
    'lifecycle': [('CPU, s', lambda r: r['target_observed_cpu_seconds']),
                  ('p95 turn, ms', lambda r: r['turn_ms']['p95']),
                  ('peak RSS, MiB', lambda r: r['daemon_peak_rss_bytes'] / 2**20)],
}


def compare(root):
    """The table's rows, the count rises that fail, and the timing warnings."""
    rows, failed, warned = [], [], []
    for name, timings in TIMINGS.items():
        base, head = runs(root, name, 'base'), runs(root, name, 'head')
        if not base or not head:
            continue
        if name == 'lifecycle':
            base = [r for r in base if r['status'] == 'ok']
            head = [r for r in head if r['status'] == 'ok']
        before, after = counts(name, base[0]), counts(name, head[0])
        for kind in sorted(set(before) | set(after)):
            was, now = before.get(kind, 0), after.get(kind, 0)
            if now > was:
                failed.append(f'{name}: {kind} rose from {was} to {now}')
            if now != was:
                rows.append((name, f'{kind} (count)', was, now, ''))
        for label, value in timings:
            try:
                was_all, now_all = [value(r) for r in base], [value(r) for r in head]
            except (KeyError, TypeError):
                # A measure one side's runs lack, as a bench run smaller.
                continue
            if None in was_all + now_all:
                # No note told on a side has no delay: say so, never read it as fast.
                rows.append((name, label, 'none' if None in was_all else statistics.median(was_all),
                             'none' if None in now_all else statistics.median(now_all), ''))
                continue
            was, now = statistics.median(was_all), statistics.median(now_all)
            change = (now - was) / was if was else 0.0
            if change > WARN and min(now_all) > max(was_all):
                warned.append(f'{name}: {label} {was:.3g} -> {now:.3g} ({change:+.0%})')
            rows.append((name, label, round(was, 3), round(now, 3), f'{change:+.1%}'))
    return rows, failed, warned


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__.strip().splitlines()[-1])
    rows, failed, warned = compare(Path(sys.argv[1]))
    table = ['| Bench | Measure | Base | Head | Change |', '| --- | --- | ---: | ---: | ---: |']
    table += [f'| {b} | {m} | {was} | {now} | {change} |' for b, m, was, now, change in rows]
    text = '\n'.join(['## Daemon and Sonny screen, base against head', '', *table, ''])
    if failed:
        text += '\nStore work per turn rose; record why in docs/DAEMON_MEASUREMENTS.md:\n'
        text += ''.join(f'- {line}\n' for line in failed)
    print(text)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as summary:
            summary.write(text + '\n')
    for line in warned:
        print(f'::warning title=Daemon screen::{line}')
    for line in failed:
        print(f'::error title=Daemon screen::{line}')
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
