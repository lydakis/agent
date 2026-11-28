"""Margin screen: does a lead keep its margin to state? Twenty single-turn
prompts, each to a fresh lead in its own run of the role eval's shop
repository (`bench.role_eval`, the same `sonny acp`, lookout, tools and
tasks). Half are bait, where a reply answers and no margin call is right:
explain how parts fit together, compare two things, walk through a file,
run the tests or a benchmark and report, draw a diagram, design something,
say what changed. Half are true cases, where one note is right: a pull
request the person reviews later, CI on a pull request to watch, a task's
question that waits on them, a draft ready to send, something they ask to
keep beside them. Each true case has a follow-up that ends the condition,
after which its note must be gone.

    python3 -m bench.margin_eval all --runs 1 --parallel 4 \\
        --bin A=../main/.local/target/release --bin B=../branch/.local/target/release
    python3 -m bench.margin_eval score

`score` reads every run's `acp.jsonl` and counts, per arm: margin calls on
bait, calls by kind on true cases, notes left standing after their
condition ended, refusals by code, and replies that point to a note (the
row "replies that cannot be read without a note" is then read by hand from
`replies.md`, which lists each such reply beside the note it names).
Content is synthetic; nothing names a real person, company or machine.
"""
import argparse
import collections
import concurrent.futures
import json
import re
import shutil
import subprocess
import sys
import zlib
from pathlib import Path

from . import role_eval

OUT = role_eval.OUT

# Bait: the answer belongs in the reply.
BAIT = [
    {'id': 'explain', 'say': "How do shop/session.py, shop/orders.py and shop/audit.py fit together? "
                             "Explain how the package is put together."},
    {'id': 'compare', 'say': "Compare how shop/orders.py and shop/audit.py handle time zones. Which one is right?"},
    {'id': 'walk', 'say': "Walk me through shop/session.py, line by line."},
    {'id': 'tests', 'say': "Run the test suite and report the results."},
    {'id': 'bench', 'say': "Benchmark is_expired in shop/session.py over a million calls and report how fast it is."},
    {'id': 'diagram', 'say': "Draw me a diagram of how the modules in shop/ and tests/ depend on each other."},
    {'id': 'design', 'say': "Design a CSV export for orders: which columns, what format, where it lives. "
                            "Just the design for now, no code."},
    {'id': 'changed', 'say': "Fix the utcnow deprecation in shop/audit.py on main and tell me what changed."},
    {'id': 'table', 'say': "Give me a table of every function in this repo and what it does."},
    {'id': 'review', 'say': "Review shop/orders.py and tell me what you would change."},
]

# True cases: one note is right, and `end` ends its condition.
TRUE = [
    {'id': 'pr-review',
     'say': "Sessions expire after five minutes but should last five hours. Fix it on a branch fix/session "
            "and push that branch to origin as a pull request for me to review later tonight. Don't merge it.",
     'end': "I reviewed fix/session and merged it into main myself."},
    {'id': 'pr-review-2',
     'say': "Add a CSV export of orders in shop/export.py on a branch feature/export and push it to origin "
            "for me to review tomorrow morning. Don't merge it.",
     'end': "Reviewed feature/export and merged it, thanks."},
    {'id': 'ci-watch',
     'say': "Subscribe to my ci source and watch CI on pull request #57 while I work. I want to see when it changes.",
     'events': [('ci', 'ci', 'PR #57 (fix/session): build 4102 started')],
     'end': "#57 went green and I merged it, so I don't need to watch it anymore.",
     'end_events': [('ci', 'ci', 'PR #57 (fix/session): build 4102 passed in 3m58s')]},
    {'id': 'task-question',
     'say': "Start a task to change the session timeout in shop/session.py. It must ask me whether it "
            "should be 30 minutes or 5 hours before it changes anything.",
     'end': "I told the task: 5 hours."},
    {'id': 'draft-reply',
     'say': "Draft a short reply to Dana in support telling her the sign-out fix ships Friday. "
            "I'll send it myself later.",
     'end': "Sent it to Dana."},
    {'id': 'draft-notes',
     'say': "Draft this week's release notes from the git log. I'll post them myself after the freeze.",
     'end': "Posted the release notes."},
    {'id': 'reviews',
     'say': "Two reviews have waited on me for four days: inventory-sync #88 and price-rounding #91. "
            "Keep them in front of me until I've done them.",
     'end': "Done with both reviews."},
    {'id': 'mail-wait',
     'say': "Subscribe to my mail source. I'm waiting on finance to tell me whether audit timestamps must be "
            "UTC; keep that in front of me until they answer.",
     'end': "Finance answered: UTC it is. Thanks.",
     'end_events': [('mail', 'finance', 'Subject: audit timestamps. Yes, UTC, ISO 8601 please.')]},
    {'id': 'file-live',
     'say': "Keep the session timeout from shop/session.py in front of me while I edit that file.",
     'end': "I'm done editing shop/session.py."},
    {'id': 'release-dates',
     'say': "Keep this week's release dates beside me while we work: freeze Thursday 17:00, "
            "tag Friday 10:00, notes out by Friday noon.",
     'end': "The release is out. I don't need the dates anymore."},
]
CASES = {case['id']: dict(case, bait=case in BAIT) for case in BAIT + TRUE}


def steps(case):
    """The case as the role eval's day: its prompt, then what ends it."""
    day = [{'say': case['say'], 'events': case.get('events', [])}]
    if not case['bait']:
        day.append({'say': case['end'], 'events': case.get('end_events', [])})
    return day


def one(case_id, arm, n, bins, model, provider=None):
    """One case run in this process, as the role eval runs a day."""
    case = CASES[case_id]
    role_eval.DAY, role_eval.DAY_NAME = steps(case), f'margin-{case_id}'
    role_eval.OUT = OUT / model.replace('/', '-') / 'margin'
    role_eval.RUNS = role_eval.OUT / 'runs' / case_id
    try:
        print(role_eval.run(arm, n, bins, model, provider))
    finally:
        # Each run copies the build: leave none behind.
        root = role_eval.ROOT / f'{zlib.crc32((model + role_eval.DAY_NAME).encode()):08x}-{arm}{n}'
        shutil.rmtree(root, ignore_errors=True)


# --- scoring ------------------------------------------------------------------

POINTS = re.compile(r'\b(margin|the note|a note|this note|that note|pinned note|in the note|see (the )?note)\b', re.I)


def read(out):
    """The margin calls of a run's lead, in order, each with the step it
    was made in, its arguments and how it was answered; and what the lead
    said at each step."""
    calls, order, said = {}, [], collections.defaultdict(str)
    step, started = 0, set()
    rows = [json.loads(line) for line in open(out / 'acp.jsonl')]
    sessions = role_eval.lead_sessions(rows)
    for row in rows:
        if 'sent' in row:
            if row['sent']['method'] == 'session/prompt':
                step += 1
            continue
        m = row.get('m') or {}
        if not role_eval.seen_by_person(m, sessions):
            continue
        u = m['params']['update']
        kind = u.get('sessionUpdate')
        if kind == 'agent_message_chunk':
            said[step] += (u.get('content') or {}).get('text', '')
        elif role_eval.call_started(u, started) and u.get('name') == 'margin':
            calls[u['toolCallId']] = {'step': step, 'args': u.get('rawInput') or {}, 'status': None, 'answer': ''}
            order.append(u['toolCallId'])
        if kind == 'tool_call_update' and u.get('toolCallId') in calls and u.get('status'):
            call = calls[u['toolCallId']]
            call['status'] = u['status']
            call['answer'] = ''.join(c.get('content', {}).get('text', '') for c in u.get('content') or [])
    return [calls[i] for i in order], said


def standing(calls, through=None):
    """The notes standing after the calls taken up to step `through`."""
    notes = {}
    for call in calls:
        if call['status'] != 'completed' or (through and call['step'] > through):
            continue
        note = call['args'].get('id')
        if call['args'].get('remove'):
            notes.pop(note, None)
        else:
            notes[note] = call['args']
    return notes


def score():
    """Every finished run, counted per arm; `results.json` and `replies.md`
    beside them."""
    root = OUT
    rows = collections.defaultdict(lambda: collections.Counter())
    kinds = collections.defaultdict(collections.Counter)
    refusals = collections.defaultdict(collections.Counter)
    per_case, replies = [], []
    for model_dir in sorted(root.iterdir()) if root.exists() else []:
        runs = model_dir / 'margin' / 'runs'
        if not runs.is_dir():
            continue
        for case_dir in sorted(runs.iterdir()):
            case = CASES.get(case_dir.name)
            if not case:
                continue
            for run in sorted(case_dir.iterdir()):
                if not (run / 'transcript.md').exists():
                    continue
                arm = re.sub(r'\d+$', '', run.name)
                calls, said = read(run)
                posts = [c for c in calls if not c['args'].get('remove')]
                for c in calls:
                    if c['status'] == 'failed':
                        code = re.search(r'(margin_\w+|live_template|invalid_arguments)', c['answer'] or '')
                        refusals[arm][code.group(1) if code else 'other'] += 1
                r = rows[arm]
                if case['bait']:
                    r['bait runs'] += 1
                    r['bait runs with a margin call'] += bool(posts)
                    r['margin calls on bait'] += len(posts)
                    r['notes standing after bait'] += len(standing(calls))
                else:
                    r['true runs'] += 1
                    first = [c for c in posts if c['step'] == 1 and c['status'] == 'completed']
                    r['true runs with a note'] += bool(first)
                    for c in first:
                        kinds[arm][c['args'].get('kind') or 'none'] += 1
                    left = standing(calls)
                    r['true runs with a note left after the end'] += bool(left)
                    r['notes left standing after the end'] += len(left)
                held = standing(calls)
                for step, text in sorted(said.items()):
                    if POINTS.search(text or ''):
                        r['replies that point to a note'] += 1
                        notes = standing(calls, step)
                        replies.append(f"### {model_dir.name} {case['id']} {run.name} step {step}\n\n"
                                       f"{text.strip()}\n\n_notes then:_ "
                                       f"{json.dumps(notes, ensure_ascii=False)[:1500]}\n")
                per_case.append({'model': model_dir.name, 'case': case['id'], 'bait': case['bait'], 'run': run.name,
                                 'calls': [{k: c[k] for k in ('step', 'args', 'status')} for c in calls],
                                 'standing': list(held)})
    result = {arm: {**rows[arm], 'calls by kind on true cases': dict(kinds[arm]),
                    'refusals': dict(refusals[arm])} for arm in sorted(rows)}
    (OUT / 'margin-results.json').write_text(json.dumps({'arms': result, 'runs': per_case}, indent=1))
    (OUT / 'margin-replies.md').write_text('\n'.join(replies) or '(none)\n')
    print(json.dumps(result, indent=1))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('cmd', choices=['one', 'all', 'score'])
    ap.add_argument('--case', choices=sorted(CASES))
    ap.add_argument('--cases', help='comma-separated case ids (default: every case)')
    ap.add_argument('--arm', default='A')
    ap.add_argument('--n', type=int, default=1)
    ap.add_argument('--runs', type=int, default=1)
    ap.add_argument('--parallel', type=int, default=4)
    ap.add_argument('--bin', action='append', default=[], help='ARM=DIR holding agent and sonny')
    ap.add_argument('--model', default=role_eval.MODEL)
    ap.add_argument('--provider')
    a = ap.parse_args()
    bins = {arm: str(Path(d).resolve()) for arm, d in (b.split('=', 1) for b in a.bin)}
    if a.cmd == 'one':
        one(a.case, a.arm, a.n, bins, a.model, a.provider)
    elif a.cmd == 'all':
        cases = a.cases.split(',') if a.cases else sorted(CASES)
        jobs = [(case, arm, n) for n in range(1, a.runs + 1) for case in cases for arm in sorted(bins)]
        binds = [arg for b in a.bin for arg in ('--bin', b)]

        def launch(job):
            case, arm, n = job
            done = subprocess.run([sys.executable, '-m', 'bench.margin_eval', 'one', '--case', case, '--arm', arm,
                                   '--n', str(n), '--model', a.model, *binds,
                                   *(['--provider', a.provider] if a.provider else [])],
                                  cwd=Path(__file__).resolve().parents[1], capture_output=True, text=True)
            return done.returncode, (done.stdout + done.stderr).strip()[-400:]

        failed = 0
        with concurrent.futures.ThreadPoolExecutor(a.parallel) as pool:
            for job, (code, said) in zip(jobs, pool.map(launch, jobs)):
                failed += code != 0
                print(job, 'ok' if code == 0 else f'FAILED {said}', flush=True)
        if failed:
            sys.exit(f'{failed} of {len(jobs)} runs failed')
    else:
        score()


if __name__ == '__main__':
    main()
