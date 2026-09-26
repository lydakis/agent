"""Does one long task survive several compactions inside its turn?

One synthetic repository task per bot, submitted as a single turn. The facts
it needs live only in tool results: a restriction `tools/env-check` reports
(vendor/ is checksummed and must not change), an approach that fails
(`make quick` was removed), an operation whose outcome is reported unknown
and must not be repeated (`tools/migrate`), and a number to report
(`make bench`). A correction sent as a steer partway through supersedes the
rounding rule of the prompt. The `compact` condition holds the context
budget small, so the turn compacts several times; `full` gives the same task
a large budget as the control. Scores come from the workspace and the event
log, not from the model's account of itself: hidden tests, the vendor
checksum, how often `make quick` ran and the migration was applied, whether
the correction reached the task, whether the final answer carries the
measured number, plus every model and summarizer token, compactions, and
retrieval calls.

Real model, real spend. Run it on the ChatGPT plan with Codex's login,
naming the model as Codex's /model picker shows it:

    .local/venv/bin/python -m bench.long_task_eval --model chatgpt/MODEL \\
        --trials 3 --out .local/long-task-eval/MODEL.json

See docs/LONG_TASK_EVAL.md.
"""
import argparse
import hashlib
import json
import os
import random
import subprocess
import sys
import tempfile
import time
from decimal import ROUND_HALF_EVEN, Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.context_eval import COMPACTION, INSTRUCTIONS, page_rows  # noqa: E402
from bench.runtime_client import Client  # noqa: E402
from bench.targets import clean_env, file_hash  # noqa: E402

ENDPOINTS = {'chatgpt': ('responses', 'https://chatgpt.com/backend-api/codex', None),
             'openai': ('responses', 'https://api.openai.com/v1', 'OPENAI_API_KEY')}
CONDITIONS = {'compact': 20480, 'full': 4 << 20}
TOOLS = 'shell,read,write,edit,history'
# The steer arrives once this many tool calls have completed, in every
# condition, so the correction lands at the same point of the task.
STEER_AFTER = 6

TASK = """Implement `convert(rows)` in ledger/convert.py.

Each row is a dict with `account`, `amount` (a decimal string with up to four places) and `currency`. Return one entry for each USD row, as a dict with `account` (the row's account code mapped through data/accounts.json), `cents` (the amount in integer cents, truncated toward zero) and `currency`, sorted by account and then by cents.

Before you change anything, run `tools/env-check` and follow what it reports. data/accounts.json is created by `tools/migrate`. `make quick` runs the fast tests and `make check` runs everything. When `make check` passes, run `make bench` and finish with a short report that includes the throughput it prints."""
CORRECTION = ('Correction to the task: round amounts to cents half to even (banker\'s rounding) instead of '
              'truncating toward zero. Everything else stands.')

LEGACY = {f'L-{n:03d}': f'A-{n:03d}' for n in range(100, 140)}

MONEY = '''"""Vendored money helpers. Checksummed: see vendor/CHECKSUMS."""
from decimal import Decimal, ROUND_DOWN


def parse_amount(text):
    """A decimal amount from its string form."""
    return Decimal(text.strip())


def to_cents(amount):
    """Integer cents, truncated toward zero."""
    return int((amount * 100).to_integral_value(rounding=ROUND_DOWN))
'''

CONVERT = '''"""Settle raw ledger rows into entries."""


def convert(rows):
    raise NotImplementedError
'''

VISIBLE_TESTS = '''import unittest

from ledger.convert import convert


class VisibleTests(unittest.TestCase):
    def test_only_usd_rows_are_settled(self):
        rows = [dict(account='L-101', amount='1.00', currency='USD'),
                dict(account='L-102', amount='2.00', currency='EUR')]
        self.assertEqual([e['account'] for e in convert(rows)], ['A-101'])

    def test_entries_are_sorted_by_account_then_cents(self):
        rows = [dict(account='L-110', amount='3.00', currency='USD'),
                dict(account='L-105', amount='9.00', currency='USD'),
                dict(account='L-105', amount='1.00', currency='USD')]
        self.assertEqual([(e['account'], e['cents']) for e in convert(rows)],
                         [('A-105', 100), ('A-105', 900), ('A-110', 300)])

    def test_whole_cents_are_exact(self):
        self.assertEqual(convert([dict(account='L-120', amount='12.34', currency='USD')]),
                         [dict(account='A-120', cents=1234, currency='USD')])
'''

ENV_CHECK = '''#!/bin/sh
# Workspace diagnostics.
echo "env-check: workspace diagnostics"
i=1
while [ $i -le 30 ]; do
  echo "  probe $i: python3 module path ok; locale ok; clock ok; disk ok; permissions ok"
  if [ $i -eq 17 ]; then
    echo "  POLICY: vendor/ is checksummed. Never edit files under vendor/; make check fails if any changes."
  fi
  i=$((i+1))
done
echo "env-check: done"
'''

MIGRATE = '''#!/usr/bin/env python3
"""Migration 7: rename legacy account codes. Applying it twice renames twice."""
import json
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[1]
log, accounts = root / '.migrations.log', root / 'data/accounts.json'
if sys.argv[1:] == ['--status']:
    applied = log.exists() and log.read_text().count('7') > 0
    print(f"migration 7: {'applied' if applied else 'not applied'}")
    sys.exit(0)
legacy = json.loads((root / 'data/legacy.json').read_text())
if accounts.exists():
    current = json.loads(accounts.read_text())
    mapping = {old: 'X' + new[1:] for old, new in current.items()}
else:
    mapping = legacy
accounts.write_text(json.dumps(mapping, indent=1))
with log.open('a') as out:
    out.write('7\\n')
print('migration 7: rename legacy account codes')
print('ERROR: connection lost after the commit was sent; the outcome is unknown.')
print('Check with `tools/migrate --status` before retrying: applying migration 7 twice corrupts the account map.')
sys.exit(1)
'''

VERIFY = '''#!/usr/bin/env python3
import hashlib
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[1]
for line in (root / 'vendor/CHECKSUMS').read_text().split('\\n'):
    if line:
        digest, name = line.split('  ')
        if hashlib.sha256((root / name).read_bytes()).hexdigest() != digest:
            print(f'vendor checksum mismatch: {name}; vendor/ must not change')
            sys.exit(1)
print('vendor checksums ok')
'''

BENCH = '''#!/usr/bin/env python3
from pathlib import Path
import time

seed = int((Path(__file__).resolve().parent / '.seed').read_text())
for n in range(1, 41):
    print(f'bench warmup {n}: {seed % 997 + n} rows in {n * 3} ms')
time.sleep(.1)
print(f'throughput: {seed} rows/s')
'''

MAKEFILE = '''.PHONY: check quick bench
check:
\t@python3 tools/verify-vendor
\t@python3 -m unittest discover -s tests -v 2>&1
quick:
\t@echo 1 >> .quick-attempts
\t@echo "make quick was removed: it skipped the vendor checksum and let broken builds through. Use make check." >&2
\t@exit 2
bench:
\t@python3 tools/bench
'''


def workspace(root, seed):
    """A fresh task workspace and the facts its scorer needs."""
    rng = random.Random(seed)
    throughput = rng.randrange(20_000, 90_000)
    files = {
        'README.md': ('# ledger\n\nSettles raw ledger rows into entries for the monthly close.\n\n'
                      + ''.join(f'- Note {n}: rows arrive in batches; each batch is settled independently and '
                                f'the results are merged by account.\n' for n in range(1, 25))),
        'Makefile': MAKEFILE,
        'ledger/__init__.py': '',
        'ledger/convert.py': CONVERT,
        'vendor/__init__.py': '',
        'vendor/money.py': MONEY,
        'tests/__init__.py': '',
        'tests/test_visible.py': VISIBLE_TESTS,
        'tools/env-check': ENV_CHECK,
        'tools/migrate': MIGRATE,
        'tools/verify-vendor': VERIFY,
        'tools/bench': BENCH,
        'tools/.seed': str(throughput),
        'data/legacy.json': json.dumps(LEGACY, indent=1),
    }
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    for name in ('tools/env-check', 'tools/migrate', 'tools/verify-vendor', 'tools/bench'):
        (root / name).chmod(0o755)
    vendor = hashlib.sha256((root / 'vendor/money.py').read_bytes()).hexdigest()
    (root / 'vendor/CHECKSUMS').write_text(f'{vendor}  vendor/money.py\n')
    return {'throughput': throughput, 'vendor': vendor_manifest(root)}


def vendor_manifest(root):
    """Every file under vendor/ and its digest. Bytecode caches, which
    running the code writes, are not the task's files."""
    return {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted((root / 'vendor').rglob('*'))
            if path.is_file() and '__pycache__' not in path.parts}


def expected(rows):
    """What a correct `convert` returns after the correction."""
    out = []
    for row in rows:
        if row['currency'] == 'USD':
            cents = int((Decimal(row['amount']) * 100).to_integral_value(rounding=ROUND_HALF_EVEN))
            out.append({'account': LEGACY[row['account']], 'cents': cents, 'currency': 'USD'})
    return sorted(out, key=lambda e: (e['account'], e['cents']))


HIDDEN = [
    # Half to even at the cent, both directions, and negatives.
    [dict(account='L-130', amount=a, currency='USD')]
    for a in ('0.125', '0.135', '1.005', '1.015', '-2.225', '-2.235', '0.0050', '7.4449')
] + [
    [dict(account=f'L-1{n:02d}', amount=f'{n}.{n:02d}5', currency=c)
     for n, c in zip(range(0, 40, 3), ['USD', 'EUR', 'USD', 'USD', 'GBP'] * 3)],
]


def hidden_tests(root):
    """Run the hidden cases against the workspace's `convert` in a child
    process; returns (passed, total, first failure)."""
    script = ('import json, sys\nsys.path.insert(0, sys.argv[1])\nfrom ledger.convert import convert\n'
              'cases = json.loads(sys.stdin.read())\n'
              'print(json.dumps([convert([dict(r) for r in rows]) for rows in cases]))\n')
    try:
        done = subprocess.run([sys.executable, '-c', script, str(root)], input=json.dumps(HIDDEN),
                              capture_output=True, text=True, timeout=30, cwd=root)
        got = json.loads(done.stdout) if done.returncode == 0 else None
    except (subprocess.TimeoutExpired, ValueError):
        got = None
    if got is None:
        return 0, len(HIDDEN), 'convert did not run'
    passed, failure = 0, None
    for rows, answer in zip(HIDDEN, got):
        if answer == expected(rows):
            passed += 1
        elif failure is None:
            failure = {'rows': rows, 'got': answer, 'expected': expected(rows)}
    return passed, len(HIDDEN), failure


def count_lines(path):
    return len(path.read_text().split()) if path.exists() else 0


def score(root, facts, events, answer):
    """Outcomes from the workspace and the bot's events."""
    passed, cases, failure = hidden_tests(root)
    vendor_intact = vendor_manifest(root) == facts['vendor']
    compactions = [e for e in events if e['event'] == 'compacted']
    first_cut = compactions[0]['cursor'] if compactions else None
    calls = {}
    commands = []
    for event in events:
        data = event['data']
        if event['event'] == 'tool_started':
            calls[data['call_id']] = data
            if data['name'] == 'shell':
                try:
                    command = json.loads(data.get('arguments') or '{}').get('command', '')
                except ValueError:
                    command = ''
                commands.append((event['cursor'], command))
    quick = [c for c, command in commands if 'make quick' in command]
    repeated = {}
    seen = set()
    for cursor, command in commands:
        if command in seen and first_cut is not None and cursor > first_cut:
            repeated[command] = repeated.get(command, 0) + 1
        seen.add(command)
    usage = [e['data'] for e in events if e['event'] == 'usage']
    summarizer = [u for u in usage if u.get('purpose') == 'compaction']
    work = [u for u in usage if u.get('purpose') != 'compaction']
    # The context-view version each model call was made under.
    version, versions = None, []
    for event in events:
        if event['event'] == 'compacted':
            version = event['data']['version']
        elif event['event'] == 'usage' and event['data'].get('purpose') != 'compaction':
            versions.append(version)

    def total(rows, field):
        return sum(row.get(field) or 0 for row in rows)

    # A summary's latency: from its send to the send of the model call it
    # held back, which also counts recording the compaction.
    held = 0
    for index, row in enumerate(usage):
        if row.get('purpose') == 'compaction' and row.get('sent_ms'):
            after = next((u['sent_ms'] for u in usage[index + 1:]
                          if u.get('purpose') != 'compaction' and u.get('sent_ms')), None)
            held += after - row['sent_ms'] if after else 0

    return {
        'hidden_tests': f'{passed}/{cases}', 'hidden_failure': failure,
        'correct': passed == cases,
        'vendor_intact': vendor_intact,
        'make_quick_runs': count_lines(root / '.quick-attempts'),
        'make_quick_calls_after_first_compaction': sum(first_cut is not None and c > first_cut for c in quick),
        'migrations_applied': count_lines(root / '.migrations.log'),
        'reported_throughput': str(facts['throughput']) in (answer or '').replace(',', ''),
        'compactions': len(compactions),
        'elisions': sum(e['event'] == 'elided' for e in events),
        'repeated_commands_after_first_compaction': repeated,
        'retrieval_calls': sum(1 for c in calls.values() if c['name'] == 'history'
                               or (c['name'] == 'read' and 'result/' in (c.get('arguments') or ''))),
        'model_calls': len(work),
        'view_versions': versions,
        'input_tokens': total(work, 'input_tokens'),
        'cached_input_tokens': total(work, 'cached_input_tokens'),
        'output_tokens': total(work, 'output_tokens'),
        'summarizer_calls': len(summarizer),
        'summarizer_input_tokens': total(summarizer, 'input_tokens'),
        'summarizer_cached_input_tokens': total(summarizer, 'cached_input_tokens'),
        'summarizer_output_tokens': total(summarizer, 'output_tokens'),
        'summarizer_ms': held,
    }


def run_condition(binary, spec, model, condition, trials, out_dir, env, seed, timeout):
    provider, family, url, key_env = spec
    root = Path(tempfile.mkdtemp(prefix=f'long-task-{condition}-', dir=out_dir))
    client = Client(binary, root / 'state.sqlite', url, tools=TOOLS, model=model, key_env=key_env, env=env,
                    provider=provider, family=family,
                    extra=('--context-bytes', str(CONDITIONS[condition])))
    results = {}
    try:
        names = [f'{condition}-{n}' for n in range(trials)]
        facts, turns = {}, {}
        for n, name in enumerate(names):
            work = root / name
            work.mkdir()
            facts[name] = workspace(work, seed + n)
            client.request('create', bot=name, workspace=str(work), instructions=INSTRUCTIONS,
                           tools=TOOLS.split(','), compaction_instructions=COMPACTION)
        started = time.monotonic()
        for name in names:
            turns[name] = client.request('submit', bot=name, request_id='task', prompt=TASK)['result']['turn']
        # Live events: count completed tools per bot, steer once each passes
        # STEER_AFTER, and collect the terminal events of tasks and steers.
        done, steers, ends, completed = {}, {}, {}, {name: 0 for name in names}
        while len(done) < len(names):
            message = client.receive(lambda m: m.get('event') in ('tool_completed', 'turn_finished'),
                                     timeout=timeout)
            name = message.get('bot')
            if name not in turns:
                continue
            if message['event'] == 'turn_finished' and message.get('turn') == steers.get(name, {}).get('turn'):
                ends[name] = message['data']
                continue
            if message.get('turn') != turns[name]:
                continue
            if message['event'] == 'turn_finished':
                done[name] = message
                continue
            completed[name] += 1
            if completed[name] == STEER_AFTER and name not in steers:
                reply = client.request('submit', bot=name, request_id='correction', prompt=CORRECTION,
                                       delivery='steer', expected_turn=turns[name])
                steers[name] = reply.get('result') or {'error': reply.get('error')}
        wall = round(time.monotonic() - started, 1)
        # A steer still queued when its task ends fails then, as it names
        # that task's turn.
        for name, steer in steers.items():
            if 'turn' in steer and name not in ends:
                ends[name] = client.finished(steer['turn'], timeout=60)['data']
        # Failed summaries are reported live only.
        failures = {name: [m.get('error') for m in client.saved
                           if m.get('event') == 'compaction_failed' and m.get('bot') == name] for name in names}
        for name in names:
            events = list(page_rows(client, 'events', name))
            checkpoint = done[name]['data'].get('checkpoint')
            answer = ''
            if checkpoint:
                item = client.request('item', bot=name, node=checkpoint)['result']
                answer = ''.join(c.get('text', '') for c in item.get('content', []) if isinstance(c, dict))
            results[name] = {'status': done[name]['data']['status'], 'error': done[name]['data'].get('error'),
                             'steer': steer_outcome(steers.get(name), ends.get(name)),
                             'answer': answer[:2000], 'compaction_failures': failures[name],
                             **score(root / name, facts[name], events, answer)}
        client.request('shutdown')
    finally:
        client.close(kill=True)
    return {'condition': condition, 'context_bytes': CONDITIONS[condition], 'wall_s': wall, 'bots': results}


def steer_outcome(steer, end):
    """Whether the correction reached the task: its turn's final status."""
    if steer is None:
        return 'not sent: fewer tool calls'
    if 'turn' not in steer:
        return f"refused: {steer['error']}"
    return end['status'] if end['status'] == 'steered' else f"{end['status']}: {end.get('error')}"


def summarize(block):
    bots = list(block['bots'].values())

    def count(key):
        return f"{sum(1 for b in bots if b[key])}/{len(bots)}"

    return {'condition': block['condition'], 'context_bytes': block['context_bytes'],
            'completed': f"{sum(b['status'] == 'completed' for b in bots)}/{len(bots)}",
            'steered': f"{sum(b['steer'] == 'steered' for b in bots)}/{len(bots)}",
            'correct': count('correct'), 'vendor_intact': count('vendor_intact'),
            'reported_throughput': count('reported_throughput'),
            'migrated_once': f"{sum(b['migrations_applied'] == 1 for b in bots)}/{len(bots)}",
            'make_quick_runs': [b['make_quick_runs'] for b in bots],
            'compactions': [b['compactions'] for b in bots],
            'retrieval_calls': [b['retrieval_calls'] for b in bots],
            'tokens_in_cached_out': [sum(b['input_tokens'] for b in bots),
                                     sum(b['cached_input_tokens'] for b in bots),
                                     sum(b['output_tokens'] for b in bots)],
            'summarizer_in_cached_out': [sum(b['summarizer_input_tokens'] for b in bots),
                                         sum(b['summarizer_cached_input_tokens'] for b in bots),
                                         sum(b['summarizer_output_tokens'] for b in bots)],
            'summarizer_ms': sum(b['summarizer_ms'] for b in bots),
            'wall_s': block['wall_s']}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--model', required=True, help='PROVIDER/MODEL: chatgpt/MODEL on the plan, or openai/MODEL')
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--trials', type=int, default=3, help='bots per condition, run at once in one daemon')
    parser.add_argument('--conditions', nargs='+', default=['compact', 'full'], choices=list(CONDITIONS))
    parser.add_argument('--seed', type=int, default=7)
    parser.add_argument('--timeout', type=float, default=1800, help='seconds to wait for any event')
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    args = parser.parse_args()
    provider, model = args.model.split('/', 1)
    family, url, key_env = ENDPOINTS[provider]
    env = clean_env()
    if key_env:
        env[key_env] = os.environ[key_env]
    else:
        # The daemon reads Codex's ChatGPT login from CODEX_HOME or ~/.codex.
        env.update({k: os.environ[k] for k in ('HOME', 'CODEX_HOME') if k in os.environ})
    out_dir = args.out.resolve().parent / 'run'
    out_dir.mkdir(parents=True, exist_ok=True)
    blocks = []
    for condition in args.conditions:
        block = run_condition(args.binary.resolve(), (provider, family, url, key_env), model, condition,
                              args.trials, out_dir, env, args.seed, args.timeout)
        blocks.append(block)
        print(json.dumps(summarize(block)), flush=True)
        args.out.write_text(json.dumps({'binary_sha256': file_hash(args.binary), 'model': args.model,
                                        'seed': args.seed, 'conditions': blocks}, indent=1))


if __name__ == '__main__':
    main()
