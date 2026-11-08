"""What the person waited on, read from their own store, read-only: turn
lengths, how long gated calls waited for each gate (the reviewer's `auto`
among them), how long compaction held turns, pacing and retries, and how big
the store is. Prints aggregates only: no prompts, no names but counts by
kind of agent (lead, task, other).

    python3 store_felt.py ~/.agent/state.sqlite --days 7
"""
import argparse
import collections
import json
import sqlite3
import sys
import time


def pct(values, q):
    if not values:
        return None
    values = sorted(values)
    return values[min(len(values) - 1, int(q * len(values)))]


def dist(values):
    return {'n': len(values), 'p50': pct(values, .5), 'p90': pct(values, .9),
            'p99': pct(values, .99), 'max': max(values) if values else None,
            'sum': sum(values)}


def kind(bot):
    if bot.endswith('.lead') or bot == 'lead' or bot.endswith('superlead'):
        return 'lead'
    if '.' in bot:
        return 'task'
    return 'other'


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('store')
    parser.add_argument('--days', type=float, default=7)
    args = parser.parse_args()
    db = sqlite3.connect(f'file:{args.store}?mode=ro', uri=True, timeout=10)
    since = int((time.time() - args.days * 86400) * 1000)
    out = {'days': args.days}

    cols = {r[1] for r in db.execute('PRAGMA table_info(turns)')}
    out['scale'] = {
        'bots': db.execute('SELECT count(*) FROM bots').fetchone()[0],
        'turns_all': db.execute('SELECT count(*) FROM turns').fetchone()[0],
        'nodes_all': db.execute('SELECT max(id) FROM nodes').fetchone()[0],
        'page_count_mib': round(db.execute('PRAGMA page_count').fetchone()[0]
                                * db.execute('PRAGMA page_size').fetchone()[0] / 2**20, 1),
    }
    # Turns in the window, by kind of agent.
    rows = db.execute(f"""SELECT bot, started_ms, finished_ms, status, model_rounds, retries,
                                 paced_ms, summary_ms, {'origin' if 'origin' in cols else 'NULL'}
                          FROM turns WHERE started_ms >= ?""", (since,)).fetchall()
    by = collections.defaultdict(lambda: collections.defaultdict(list))
    statuses = collections.Counter()
    for bot, started, finished, status, rounds, retries, paced, summary, origin in rows:
        k = kind(bot)
        statuses[(k, status)] += 1
        if finished:
            by[k]['turn_ms'].append(finished - started)
        by[k]['rounds'].append(rounds or 0)
        if paced:
            by[k]['paced_ms'].append(paced)
        if summary:
            by[k]['summary_ms'].append(summary)
            if finished:
                by[k]['summary_share'].append(round(summary / max(1, finished - started), 3))
        if retries:
            by[k]['retries'].append(retries)
        by[k]['origin:' + str(origin)].append(1)
    out['turns'] = {k: {name: (dist(v) if not name.startswith('origin:') else len(v))
                        for name, v in sorted(d.items())} for k, d in by.items()}
    out['statuses'] = {f'{k}/{s}': n for (k, s), n in statuses.most_common()}

    # Events in the window: which kinds, and the gates' waits.
    span = db.execute('SELECT min(written_ms), max(written_ms), count(*) FROM events WHERE written_ms >= ?',
                      (since,)).fetchone()
    out['events_span'] = {'first_ms': span[0], 'last_ms': span[1], 'count': span[2],
                          'oldest_kept_ms': db.execute('SELECT min(written_ms) FROM events').fetchone()[0]}
    out['event_kinds'] = dict(db.execute(
        'SELECT kind, count(*) FROM events WHERE written_ms >= ? GROUP BY kind ORDER BY 2 DESC',
        (since,)).fetchall())
    gates = collections.defaultdict(list)
    per_turn = collections.defaultdict(int)
    gated_calls = 0
    for bot, turn, data in db.execute(
            "SELECT bot, turn, data FROM events WHERE kind='tool_started' AND written_ms >= ?", (since,)):
        try:
            approvals = json.loads(data).get('approvals') or []
        except ValueError:
            continue
        if approvals:
            gated_calls += 1
        longest = 0
        for a in approvals:
            waited = a.get('waited_ms')
            if waited is None:
                continue
            gates[f"{kind(bot)}:{a.get('tag')}:{a.get('by')}"].append(waited)
            longest = max(longest, waited)
        if approvals:
            per_turn[(bot, turn)] += longest
    out['gated_calls'] = gated_calls
    out['gate_waits_ms'] = {k: dist(v) for k, v in sorted(gates.items(), key=lambda kv: -len(kv[1]))}
    # The share of each gated turn's length spent waiting on gates.
    lengths = {(b, t): f - s for b, t, s, f in db.execute(
        'SELECT bot, id, started_ms, finished_ms FROM turns WHERE started_ms >= ? AND finished_ms IS NOT NULL',
        (since,))}
    shares = [round(w / max(1, lengths[key]), 3) for key, w in per_turn.items() if key in lengths]
    out['gate_share_of_turn'] = dist(shares)
    out['gate_ms_per_gated_turn'] = dist(list(per_turn.values()))

    # Approvals rows: announced to last verdict, by gate, as a cross-check.
    acols = {r[1] for r in db.execute('PRAGMA table_info(approvals)')}
    if 'verdicts' in acols:
        waits = collections.defaultdict(list)
        for announced, verdicts in db.execute(
                'SELECT announced_ms, verdicts FROM approvals WHERE announced_ms >= ? AND verdicts IS NOT NULL',
                (since,)):
            try:
                for v in json.loads(verdicts) or []:
                    at = v.get('at_ms')
                    if at:
                        allow = 'allow' if v.get('allow') else 'deny'
                        waits[f"{v.get('tag')}:{v.get('by')}:{allow}"].append(max(0, at - announced))
            except (ValueError, AttributeError):
                continue
        out['approval_rows_ms'] = {k: dist(v) for k, v in sorted(waits.items(), key=lambda kv: -len(kv[1]))}

    # Compactions: how many, how much each kept, per bot kind (no text).
    ccols = {r[1] for r in db.execute('PRAGMA table_info(compactions)')}
    out['compactions_all'] = db.execute('SELECT count(*) FROM compactions').fetchone()[0]
    out['compaction_columns'] = sorted(ccols)
    json.dump(out, sys.stdout, indent=1)
    print()


if __name__ == '__main__':
    main()
