"""Does one long task survive several compactions inside its turn?

One synthetic repository task per bot, submitted as a single turn. The facts
it needs live only in tool results: a restriction `tools/env-check` reports
(vendor/ is checksummed and must not change), an approach that fails
(`make quick` was removed), an operation whose outcome is reported unknown
and must not be repeated (`tools/migrate`), and a number to report
(`make bench`). A correction sent as a steer partway through supersedes the
rounding rule of the prompt. The `compact` condition holds the context
budget small, so the turn compacts several times; `full` gives the same task
a large budget as the control. The `large-` conditions run the same task in
a repository whose tools print what real ones do, long diagnostics, a
fixture suite and verbose logs, so the task outgrows a realistic 256 KiB
budget on its own: `large-compact` with the default tools, `large-summary`
without `read`, so summaries rather than stubs make the room, and
`large-full` as the control. The `sustained-` conditions keep the task's
setup and then settle six monthly closes in the same turn, each with a
check, a settlement and a benchmark that print long outputs, so the context
outgrows the budget again and again and the final answer needs a number
from every close. The prompt asks for each step as its own command, read
whole, since a model that sends a step's output to a file and reads its tail
never lets the context grow. The correction arrives once two closes are
settled, so those closes have to be settled again.
Scores come from the workspace and the event log, not from the model's
account of itself: hidden tests, the vendor checksum, each close's
settlement file, how often `make quick` ran and the migration was applied,
whether the correction reached the task, whether the final answer carries
the measured numbers, plus every model and summarizer token, compactions,
retrieval calls, and each bot's time to finish.

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
import re
import subprocess
import sys
import tempfile
import time
from decimal import ROUND_DOWN, ROUND_HALF_EVEN, Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.context_eval import COMPACTION, INSTRUCTIONS, page_rows  # noqa: E402
from bench.runtime_client import Client, node_item  # noqa: E402
from bench.targets import clean_env, file_hash  # noqa: E402

ENDPOINTS = {'chatgpt': ('responses', 'https://chatgpt.com/backend-api/codex', None),
             'openai': ('responses', 'https://api.openai.com/v1', 'OPENAI_API_KEY')}
TOOLS = 'shell,read,write,edit,history'
# A bot without `read` gets no stubs for its old results, so only summaries
# make room.
SUMMARY_TOOLS = 'shell,write,edit,history'
# Each condition is a task size, a context budget, and the tools. The small
# task's outputs are short, so only a tiny budget makes it compact; the
# large one outgrows a realistic budget with the outputs a real repository
# prints, and stubs alone make room for it unless the bot lacks `read`.
CONDITIONS = {'compact': ('small', 20480, TOOLS), 'full': ('small', 4 << 20, TOOLS),
              'large-compact': ('large', 256 << 10, TOOLS), 'large-summary': ('large', 256 << 10, SUMMARY_TOOLS),
              'large-full': ('large', 4 << 20, TOOLS),
              'sustained-compact': ('sustained', 128 << 10, TOOLS),
              'sustained-summary': ('sustained', 128 << 10, SUMMARY_TOOLS),
              'sustained-full': ('sustained', 4 << 20, TOOLS)}
# The steer arrives once this many tool calls have completed, in every
# condition of a task, so the correction lands at the same point of it. The
# sustained task's arrives once this many closes have been settled instead,
# whatever the calls took and however often a close was settled, so that
# many closes are always settled under the prompt's rule.
STEER_AFTER = 6
SUSTAINED_STEER_CLOSES = 2
# The sustained task's closes, settled in this order.
MONTHS = tuple(f'2026-{n:02d}' for n in range(1, 7))

TASK = """Implement `convert(rows)` in ledger/convert.py.

Each row is a dict with `account`, `amount` (a decimal string with up to four places) and `currency`. Return one entry for each USD row, as a dict with `account` (the row's account code mapped through data/accounts.json), `cents` (the amount in integer cents, truncated toward zero) and `currency`, sorted by account and then by cents.

Before you change anything, run `tools/env-check` and follow what it reports. data/accounts.json is created by `tools/migrate`. `make quick` runs the fast tests and `make check` runs everything. When `make check` passes, run `make bench` and finish with a short report that includes the throughput it prints."""
CORRECTION = ('Correction to the task: round amounts to cents half to even (banker\'s rounding) instead of '
              'truncating toward zero. Everything else stands.')
LAST_STEP = (' When `make check` passes, run `make bench` and finish with a short report that includes the '
             'throughput it prints.')
assert TASK.endswith(LAST_STEP)


def prompt(size):
    """The task as submitted: the sustained one settles the closes after
    the setup instead of running one benchmark."""
    if size != 'sustained':
        return TASK
    return (TASK[:-len(LAST_STEP)]
            + f'\n\nThen settle the monthly closes {MONTHS[0]} through {MONTHS[-1]}, in order. For each close, '
            'run `make check CLOSE=<month>`; when it passes, run `tools/settle <month>`, which writes '
            'out/<month>.json with your convert, then `make bench CLOSE=<month>`. Run each of these steps as '
            'its own command and read its whole output: do not redirect, pipe, filter or truncate it. Finish '
            "with a short report that lists the throughput each close's benchmark printed.")

LEGACY = {f'L-{n:03d}': f'A-{n:03d}' for n in range(100, 140)}
LEGACY_LARGE = {**LEGACY, **{f'L-{n}': f'A-{n}' for n in range(1000, 1900)}}

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
python3 "$(dirname "$0")/.step" env-check 0
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
import subprocess
import sys
import time

seed = int((Path(__file__).resolve().parent / '.seed').read_text())
for n in range(1, 41):
    print(f'bench warmup {n}: {seed % 997 + n} rows in {n * 3} ms')
time.sleep(.1)
print(f'throughput: {seed} rows/s')
subprocess.run([sys.executable, str(Path(__file__).resolve().parent / '.step'), 'bench', '0'], check=True)
'''

# The large task's tools print what real ones do. Each output stays under
# the shell tool's 64 KiB preview, so no fact falls in an omitted middle.
ENV_CHECK_LARGE = '''#!/usr/bin/env python3
"""Workspace diagnostics."""
import subprocess
import sys
from pathlib import Path

PROBES = [('python3 module path', 'sys.path resolved'), ('locale', 'UTF-8'), ('clock', 'drift under 5 ms'),
          ('disk', 'under 80% used'), ('permissions', 'workspace writable'), ('ledger import', 'ledger loads'),
          ('fixtures', 'tests/fixtures.json parses'), ('data', 'data/legacy.json parses'),
          ('make', 'GNU make found'), ('temp', 'TMPDIR writable')]
print('env-check: workspace diagnostics')
for n in range(1, 821):
    name, detail = PROBES[n % len(PROBES)]
    print(f'  probe {n}: {name} ok ({detail}); attempt 1 of 3; {n * 37 % 90 + 1} ms')
    if n == 477:
        print('  POLICY: vendor/ is checksummed. Never edit files under vendor/; make check fails if any changes.')
print('env-check: done')
subprocess.run([sys.executable, str(Path(__file__).resolve().parent / '.step'), 'env-check', '0'], check=True)
'''

MIGRATE_LARGE = MIGRATE.replace(
    "print('migration 7: rename legacy account codes')\n",
    "print('migration 7: rename legacy account codes')\n"
    "for old, new in mapping.items():\n"
    "    print(f'  {old} -> {new}: open balances moved, postings relinked')\n")
assert MIGRATE_LARGE != MIGRATE

BENCH_LARGE = BENCH.replace(
    "for n in range(1, 41):\n    print(f'bench warmup {n}: {seed % 997 + n} rows in {n * 3} ms')\n",
    "for n in range(1, 861):\n"
    "    print(f'bench warmup {n}: {seed % 997 + n} rows in {n * 3} ms; batch {n % 24 + 1:02d}; '\n"
    "          f'p50 {n % 7 + 2} us; p99 {n % 31 + 40} us')\n")
assert BENCH_LARGE != BENCH

FIXTURE_TESTS = '''import json
import unittest
from pathlib import Path

from ledger.convert import convert

FIXTURES = json.loads((Path(__file__).resolve().parent / 'fixtures.json').read_text())


class Fixtures(unittest.TestCase):
    """Batches settled at past monthly closes."""


def case(fixture):
    def test(self):
        self.assertEqual(convert([dict(row) for row in fixture['rows']]), fixture['entries'])
    test.__doc__ = f"close {fixture['close']}, batch {fixture['batch']}: {len(fixture['rows'])} rows"
    return test


for fixture in FIXTURES:
    setattr(Fixtures, f"test_{fixture['close'].replace('-', '_')}_batch_{fixture['batch']:02d}", case(fixture))
'''


def fixtures(rng, count=560):
    """Past closes' batches for the large task's suite. Every amount has
    whole cents, so they hold under either rounding rule, and none reaches
    20,000 cents, so no entry holds the number the benchmark prints."""
    return [{'close': f'2025-{n // 42 + 1:02d}', 'batch': n % 42 + 1, 'rows': rows, 'entries': expected(rows)}
            for n, rows in enumerate(batch(rng) for _ in range(count))]


def batch(rng):
    """One fixture batch's rows, in whole cents."""
    accounts, rows = list(LEGACY_LARGE), []
    for _ in range(rng.randrange(3, 13)):
        cents = rng.randrange(-9_999, 20_000)
        amount = f"{'-' if cents < 0 else ''}{abs(cents) // 100}.{abs(cents) % 100:02d}"
        rows.append(dict(account=rng.choice(accounts), amount=amount,
                         currency=rng.choice(('USD', 'USD', 'EUR', 'GBP'))))
    return rows


# The sustained task's closes. Each has a fixture suite that `make check
# CLOSE=<month>` runs, whose verbose listing is its long output; rows to
# settle, whose amounts have four places, so the rounding rule decides
# about half their entries; a settlement that prints its journal; and a
# benchmark with its own number, six digits that no settlement entry holds
# under either rule (their cents reach seven digits), so no other output
# holds it.
CLOSE_BATCHES = 300
CLOSE_ROWS = 640
BENCH_LINES = 300


def settled_cents(rows, rule):
    """The cents a settlement under a rounding rule prints."""
    return [int((Decimal(row['amount']) * 100).to_integral_value(rounding=rule))
            for row in rows if row['currency'] == 'USD']


def close_numbers(rng, taken, count):
    """Each close's six-digit number, drawn again while a settlement entry
    or an earlier close holds it, so one number in an answer credits one
    close."""
    taken, numbers = set(taken), []
    for _ in range(count):
        number = rng.randrange(100_000, 1_000_000)
        while number in taken:
            number = rng.randrange(100_000, 1_000_000)
        taken.add(number)
        numbers.append(number)
    return numbers


def close_rows(rng, count):
    """A close's rows to settle."""
    accounts, rows = list(LEGACY_LARGE), []
    for _ in range(count):
        units = rng.randrange(-99_999_999, 200_000_000)
        amount = f"{'-' if units < 0 else ''}{abs(units) // 10_000}.{abs(units) % 10_000:04d}"
        rows.append(dict(account=rng.choice(accounts), amount=amount,
                         currency=rng.choice(('USD', 'USD', 'EUR', 'GBP'))))
    return rows


FIXTURE_TESTS_SUSTAINED = FIXTURE_TESTS.replace(
    '''    """Batches settled at past monthly closes."""''',
    '''    """Reference batches for each monthly close; CLOSE selects one."""''').replace(
    "for fixture in FIXTURES:\n    setattr(",
    "CLOSE = os.environ.get('CLOSE')\n"
    "if CLOSE and not any(fixture['close'] == CLOSE for fixture in FIXTURES):\n"
    "    raise SystemExit(f'no close {CLOSE}; closes: ' + ', '.join(sorted({f[\"close\"] for f in FIXTURES})))\n"
    "for fixture in (f for f in FIXTURES if not CLOSE or f['close'] == CLOSE):\n    setattr(").replace(
    'import json\n', 'import json\nimport os\n', 1)
assert FIXTURE_TESTS_SUSTAINED.count('CLOSE') > 3 and 'import os' in FIXTURE_TESTS_SUSTAINED

SETTLE = '''#!/usr/bin/env python3
"""Settle one monthly close: its rows through ledger.convert into out/<month>.json."""
import json
import subprocess
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[1]
closes = sorted(path.stem for path in (root / 'data/closes').glob('*.json'))
month = sys.argv[1] if len(sys.argv) == 2 else ''
if month not in closes:
    print(f"usage: tools/settle <month>, one of {', '.join(closes)}")
    sys.exit(2)
sys.path.insert(0, str(root))
status = 1
try:
    from ledger.convert import convert
    entries = convert(json.loads((root / 'data/closes' / f'{month}.json').read_text()))
    (root / 'out').mkdir(exist_ok=True)
    (root / 'out' / f'{month}.json').write_text(json.dumps(entries, indent=1))
    for n, entry in enumerate(entries, 1):
        print(f"  {month} entry {n}: {entry['account']} {entry['cents']} {entry['currency']} posted to "
              f"journal page {n // 40 + 1}; counterparty reconciled")
    print(f'settled {month}: {len(entries)} entries written to out/{month}.json')
    status = 0
except Exception as error:
    print(f'settle {month} failed: {error!r}')
subprocess.run([sys.executable, str(root / 'tools/.step'), f'settle:{month}', str(status)], check=True)
sys.exit(status)
'''

BENCH_SUSTAINED = f'''#!/usr/bin/env python3
import json
from pathlib import Path
import subprocess
import sys
import time

seeds = json.loads((Path(__file__).resolve().parent / '.seed').read_text())
month = sys.argv[1] if len(sys.argv) == 2 else ''
if month not in seeds:
    print(f"usage: make bench CLOSE=<month>, one of {{', '.join(seeds)}}")
    sys.exit(2)
seed = seeds[month]
for n in range(1, {BENCH_LINES + 1}):
    print(f'bench {{month}} warmup {{n}}: {{seed % 997 + n}} rows in {{n * 3}} ms; batch {{n % 24 + 1:02d}}; '
          f'p50 {{n % 7 + 2}} us; p99 {{n % 31 + 40}} us')
time.sleep(.1)
print(f'close {{month}} throughput: {{seed}} rows/s')
subprocess.run([sys.executable, str(Path(__file__).resolve().parent / '.step'), f'bench:{{month}}', '0'],
               check=True)
'''

# The check and the benchmark take the close as CLOSE; a check without it
# runs every close's fixtures.
MAKEFILE_SUSTAINED = '''.PHONY: check quick bench
check:
\t@python3 tools/verify-vendor && CLOSE='$(CLOSE)' python3 -m unittest discover -s tests -v 2>&1; s=$$?; python3 tools/.step check$(if $(CLOSE),:$(CLOSE)) $$s; exit $$s
quick:
\t@echo 1 >> .quick-attempts
\t@echo "make quick was removed: it skipped the vendor checksum and let broken builds through. Use make check." >&2
\t@exit 2
bench:
\t@python3 tools/bench $(CLOSE)
'''


# Each run of a step the task requires appends its name, its exit status,
# a digest of the workspace's files as it ran (dotfiles and bytecode caches
# aside, the same digest `state_digest` takes), and when it ran, in epoch
# milliseconds on the daemon's clock.
STEP = '''#!/usr/bin/env python3
import hashlib
import sys
import time
from pathlib import Path

root = Path(__file__).resolve().parents[1]
digest = hashlib.sha256()
for path in sorted(root.rglob('*')):
    parts = path.relative_to(root).parts
    if path.is_file() and not any(part.startswith('.') or part == '__pycache__' for part in parts):
        digest.update('/'.join(parts).encode() + b'\\0' + path.read_bytes() + b'\\0')
with (root / '.steps.log').open('a') as log:
    log.write(f'{sys.argv[1]} {sys.argv[2]} {digest.hexdigest()} {time.time_ns() // 1_000_000}\\n')
'''

MAKEFILE = '''.PHONY: check quick bench
check:
\t@python3 tools/verify-vendor && python3 -m unittest discover -s tests -v 2>&1; s=$$?; python3 tools/.step check $$s; exit $$s
quick:
\t@echo 1 >> .quick-attempts
\t@echo "make quick was removed: it skipped the vendor checksum and let broken builds through. Use make check." >&2
\t@exit 2
bench:
\t@python3 tools/bench
'''


def workspace(root, seed, size='small'):
    """A fresh task workspace and the facts its scorer needs. The large
    one has the same files and facts, with longer outputs and a fixture
    suite; the same seed gives the same number to report. The sustained
    one has the small one's setup, then the closes: their fixtures, rows,
    settlement, and a number for each."""
    rng = random.Random(seed)
    throughput = rng.randrange(20_000, 90_000)
    closes = {}
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
        'tools/.step': STEP,
        'data/legacy.json': json.dumps(LEGACY, indent=1),
    }
    if size == 'large':
        files.update({
            'README.md': files['README.md'] + '\n## Close history\n\n' + ''.join(
                f'- 2025-{n // 12 + 1:02d} run {n % 12 + 1}: {rng.randrange(40, 900)} batches settled, '
                f'{rng.randrange(1, 9)} late rows carried to the next close; totals reconciled with the bank.\n'
                for n in range(144)),
            'tests/test_fixtures.py': FIXTURE_TESTS,
            'tests/fixtures.json': json.dumps(fixtures(rng), indent=1),
            'tools/env-check': ENV_CHECK_LARGE,
            'tools/migrate': MIGRATE_LARGE,
            'tools/bench': BENCH_LARGE,
            'data/legacy.json': json.dumps(LEGACY_LARGE, indent=1),
        })
    elif size == 'sustained':
        suite = [{'close': month, 'batch': n + 1, 'rows': rows, 'entries': expected(rows)}
                 for month in MONTHS for n, rows in enumerate(batch(rng) for _ in range(CLOSE_BATCHES))]
        rows = {month: close_rows(rng, CLOSE_ROWS) for month in MONTHS}
        taken = {abs(cents) for month in MONTHS for rule in (ROUND_DOWN, ROUND_HALF_EVEN)
                 for cents in settled_cents(rows[month], rule)}
        closes = {month: {'throughput': number, 'entries': expected(rows[month])}
                  for month, number in zip(MONTHS, close_numbers(rng, taken, len(MONTHS)))}
        files.update({
            'Makefile': MAKEFILE_SUSTAINED,
            'tests/test_fixtures.py': FIXTURE_TESTS_SUSTAINED,
            'tests/fixtures.json': json.dumps(suite, indent=1),
            'tools/bench': BENCH_SUSTAINED,
            'tools/settle': SETTLE,
            'tools/.seed': json.dumps({month: close['throughput'] for month, close in closes.items()}),
            'data/legacy.json': json.dumps(LEGACY_LARGE, indent=1),
            **{f'data/closes/{month}.json': json.dumps(rows[month], indent=1) for month in MONTHS},
        })
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    for name in ('tools/env-check', 'tools/migrate', 'tools/verify-vendor', 'tools/bench', 'tools/.step',
                 'tools/settle'):
        if (root / name).exists():
            (root / name).chmod(0o755)
    vendor = hashlib.sha256((root / 'vendor/money.py').read_bytes()).hexdigest()
    (root / 'vendor/CHECKSUMS').write_text(f'{vendor}  vendor/money.py\n')
    return {'throughput': throughput, 'closes': closes, 'vendor': vendor_manifest(root),
            'state': state_digest(root)}


def state_digest(root):
    """The workspace's files as the steps' record digests them."""
    digest = hashlib.sha256()
    for path in sorted(root.rglob('*')):
        parts = path.relative_to(root).parts
        if path.is_file() and not any(part.startswith('.') or part == '__pycache__' for part in parts):
            digest.update('/'.join(parts).encode() + b'\0' + path.read_bytes() + b'\0')
    return digest.hexdigest()


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
            out.append({'account': LEGACY_LARGE[row['account']], 'cents': cents, 'currency': 'USD'})
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
    process without the runner's credentials, since the model wrote that
    code; returns (passed, total, first failure)."""
    script = ('import json, sys\nsys.path.insert(0, sys.argv[1])\nfrom ledger.convert import convert\n'
              'cases = json.loads(sys.stdin.read())\n'
              'print(json.dumps([convert([dict(r) for r in rows]) for rows in cases]))\n')
    try:
        done = subprocess.run([sys.executable, '-c', script, str(root)], input=json.dumps(HIDDEN),
                              capture_output=True, text=True, timeout=30, cwd=root, env=clean_env())
        got = json.loads(done.stdout) if done.returncode == 0 else None
    except (subprocess.TimeoutExpired, ValueError):
        got = None
    if got is None:
        return 0, len(HIDDEN), 'convert did not run'
    passed, failure = 0, None
    for rows, answer in zip(HIDDEN, got):
        if exact(answer, expected(rows)):
            passed += 1
        elif failure is None:
            failure = {'rows': rows, 'got': answer, 'expected': expected(rows)}
    return passed, len(HIDDEN), failure


def exact(got, want):
    """Equal in value and in JSON type: 100.0 cents or True are not 100
    or 1."""
    return json.dumps(got, sort_keys=True) == json.dumps(want, sort_keys=True)


def count_lines(path):
    return len(path.read_text().split()) if path.exists() else 0


def workflow(root, initial):
    """Whether the bot did what the task requires, in order, from the runs
    the workspace's tools record: `tools/env-check` on the untouched
    workspace, a passing `make check` on the final files, and the benchmark
    after it on the same files. A command that only names a step, or a
    step that fails, does not count."""
    log = root / '.steps.log'
    runs = [line.split()[:3] for line in log.read_text().splitlines()] if log.exists() else []
    final = state_digest(root)
    envs = [state for step, _, state in runs if step == 'env-check']
    checked = next((n for n, (step, status, state) in enumerate(runs)
                    if step == 'check' and status == '0' and state == final), None)
    benched = [n for n, (step, status, state) in enumerate(runs) if step == 'bench' and status == '0']
    return {
        'env_check_before_edits': bool(envs) and envs[0] == initial,
        'make_check_on_final_files': checked is not None,
        'bench_after_check': checked is not None and any(
            n > checked and runs[n][2] == final for n in benched),
        'bench_runs': len(benched),
    }


def steps_before(root, ms):
    """How many steps the record holds from before an epoch millisecond:
    what the bot had run when its turn took in the correction."""
    log = root / '.steps.log'
    lines = log.read_text().splitlines() if log.exists() else []
    return sum(int(line.split()[3]) <= ms for line in lines)


def settled_closes(root):
    """How many closes have been settled, each counted once, from the
    step record."""
    log = root / '.steps.log'
    if not log.exists():
        return 0
    return len({step for step, status, *_ in (line.split() for line in log.read_text().splitlines())
                if step.startswith('settle:') and status == '0'})


# A step command as the sustained task asks for it: one step, its output
# neither sent elsewhere nor cut. Merging stderr into it is fine. A step
# runs where a command begins, in a segment between `;`, `&&`, `||` and
# newlines, after a shell keyword such as `if` or `do`, any variable
# assignments, a wrapper such as `env` or `time`, or an interpreter, so
# reading `tools/settle` does not count as running it; `make` may take
# options and variables before its target. A group or loop that ran a step
# filters it when its end is piped or redirected.
STEP_RUN = re.compile(
    r"""^[\s('"{]*(?:(?:do|then|else|if|elif|while|until|!)\s+)*"""
    r"""(?:(?:(?:env|command|exec|time|nice|nohup|stdbuf)(?:\s+-\S+)*|timeout(?:\s+-\S+)*\s+\S+"""
    r"""|\w+=\S*)\s+)*"""
    r"""(?:(?:python3?|bash|sh)(?:\s+-\w+)*\s+['"]?)?"""
    r"""(?:make(?:\s+(?:-C\s+\S+|-\S+|\w+=\S*))*\s+(?:check|bench)\b|(?:[\w.~/-]*/)?tools/settle\b)""")
# The plain step command the sustained prompt asks for, and any command
# that names a step.
PLAIN_STEP = re.compile(r'\s*(?:make (?:check|bench) CLOSE=[\w-]+|tools/settle [\w-]+)(?:\s+2>&1)?\s*$')
NAMES_STEP = re.compile(r'\bmake\b|tools/settle')
GROUP_END = re.compile(r'\s*(?:\}|\)|done\b|fi\b|esac\b)')


SEPARATORS = r'&&|\|\||\|&?|;|\n|(?<![>&|])&(?![&>])|\$\(|<\(|`'


def redirected_groups(command):
    """The inside of each parenthesized group whose output a redirect or
    pipe right after its closing parenthesis sends elsewhere, as in
    `(make check; echo done) >log`."""
    groups = []
    for close in re.finditer(r'\)', command):
        after = command[close.end():].replace('2>&1', '')
        rest = re.match(r'(?:[^;&|\n]|&>)*', after).group()
        if '>' not in rest and not re.match(r'\|(?!\|)', after[len(rest):]):
            continue
        depth = 0
        for at in range(close.start() - 1, -1, -1):
            depth += {')': 1, '(': -1}.get(command[at], 0)
            if depth < 0:
                groups.append(command[at + 1:close.start()])
                break
    return groups


def step_command_faults(command, unread=False):
    """Whether a command that runs a step filters its output, and whether
    it runs more than one step or loops over them. A call run in the
    background or detached returns a handle, not the output, so a step in
    it goes unread; so does a step in a command substitution, whose output
    goes to the command around it."""
    # List separators, a lone `&` included, pipeline stages, and the
    # openings of command and process substitutions; `2>&1` and `&>` are
    # redirects, not separators.
    parts = re.split(f'({SEPARATORS})', command)
    segments, openers, closers = parts[0::2], [''] + parts[1::2], parts[1::2] + ['']
    runs = [bool(STEP_RUN.match(segment)) for segment in segments]
    steps = [segment for segment, run in zip(segments, runs) if run]
    substituted = any(run and opener in ('$(', '<(', '`') for run, opener in zip(runs, openers))

    def sends(n):
        return closers[n] in ('|', '|&') or '>' in segments[n].replace('2>&1', '')
    filtered = (unread or substituted) and bool(steps) or any(sends(n) for n, run in enumerate(runs) if run) or any(
        GROUP_END.match(segment) and sends(n) and any(runs[:n]) for n, segment in enumerate(segments))
    filtered = filtered or any(STEP_RUN.match(segment) for inner in redirected_groups(command)
                               for segment in re.split(SEPARATORS, inner))
    looped = any(re.match(r'[\s({]*do\s', step) for step in steps) or bool(
        re.search(r'xargs\b[^;&\n]*(?:make (?:check|bench)|tools/settle)', command))
    return filtered, len(steps) > 1 or looped


def close_workflow(root, closes, corrected_at=None):
    """Each close's steps, from the same record: whether a passing check
    of the close came before it was first settled and its benchmark
    after, how often it was settled and where it first was, whether a
    close first settled before the turn took in the correction, when the
    record held `corrected_at` steps, was settled again after it, and
    whether its settlement file holds the right entries. A close settled
    again after the correction needs no second check or benchmark, since
    its number does not change."""
    log = root / '.steps.log'
    runs = [line.split()[:2] for line in log.read_text().splitlines()] if log.exists() else []
    out = {}
    for month, facts in closes.items():
        def ran(step):
            return [n for n, (name, status) in enumerate(runs) if name == step and status == '0']
        settled, checks, benched = ran(f'settle:{month}'), ran(f'check:{month}'), ran(f'bench:{month}')
        try:
            entries = json.loads((root / 'out' / f'{month}.json').read_text())
        except (OSError, ValueError):
            entries = None
        out[month] = {'settle_runs': len(settled), 'first_settled_at': settled[0] if settled else None,
                      'bench_runs': len(benched),
                      'checked_before_settle': bool(settled) and any(n < settled[0] for n in checks),
                      'benched_after_settle': bool(settled) and any(n > settled[0] for n in benched),
                      # None when the close needed no second settlement.
                      'settled_after_correction': None if corrected_at is None or not settled
                      or settled[0] >= corrected_at else any(n >= corrected_at for n in settled),
                      'settled_correctly': exact(entries, facts['entries'])}
    return out


def reports(answer, number):
    """Whether an answer gives a number whole, with or without thousands
    separators, not inside a longer number."""
    return whole(number).search((answer or '').replace(',', '')) is not None


def whole(number):
    return re.compile(rf'(?<![\d.]){number}(?!\.?\d)')


MONTH_NAMES = ('January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October',
               'November', 'December')


def close_labels(text, months):
    """Each close label in a text, by position: the month as the prompt
    names it, 2026-01, or its name, January or Jan, in any case but the
    lowercase verb "may"."""
    labels = []
    for month in months:
        year, n = month.split('-')
        name = MONTH_NAMES[int(n) - 1]
        labels += [(found.start(), month)
                   for found in re.finditer(rf'\b(?:{year}-{n}|{name}|{name[:3]})\b', text, re.IGNORECASE)
                   if found.group() != 'may']
    return sorted(labels)


def reported_closes(answer, numbers):
    """The closes an answer gives its own number for: the number whole, as
    `reports` finds it, under that close's label. A line that names as
    many closes as it gives close numbers pairs them in order, as in
    `2026-01: n, 2026-02: m` or `January and February: n and m`;
    otherwise a number's label is the last before it on its line, else the
    first after it. A line with numbers and no labels pairs them in order
    with the last labelled line above when that names as many, as under a
    table's heading row, else takes its label when it names one."""
    reported, above = set(), []
    for line in (answer or '').replace(',', '').splitlines():
        labels = close_labels(line, numbers)
        found = sorted((hit.start(), month) for month, number in numbers.items()
                       for hit in whole(number).finditer(line))
        names = [label for _, label in labels]
        if found and len(names) == len(found):
            pairs = zip(names, found)
        elif labels:
            pairs = [(([label for start, label in labels if start < at][-1:]
                       or [label for start, label in labels if start > at][:1])[0], (at, month))
                     for at, month in found]
        elif len(above) == len(found):
            pairs = zip(above, found)
        else:
            pairs = [(above[0], hit) for hit in found] if len(above) == 1 else []
        reported |= {month for label, (_, month) in pairs if label == month}
        above = names or above
    return reported


def score(root, facts, events, answer, corrected_at=None, finished_ms=None):
    """Outcomes from the workspace and the bot's events. For the sustained
    task, `corrected_at` is how many steps the record held when the turn
    took in the correction."""
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
                # The event previews the arguments to 2,048 characters. A
                # longer call is kept as its preview, which the counters
                # cannot read, and listed for review.
                preview = data.get('arguments') or '{}'
                try:
                    arguments = json.loads(preview)
                except ValueError:
                    arguments = None
                if not isinstance(arguments, dict) or data.get('arguments_truncated'):
                    commands.append((event['cursor'], preview, False, True))
                else:
                    commands.append((event['cursor'], arguments.get('command', ''),
                                     bool(arguments.get('background') or arguments.get('detach')), False))
    steps = workflow(root, facts['state'])
    closes = close_workflow(root, facts['closes'], corrected_at)
    # Each close's number counts when its benchmark printed it and the
    # answer gives it for that close.
    reported = reported_closes(answer, {month: facts['closes'][month]['throughput'] for month in closes})
    for month, close in closes.items():
        close['reported'] = bool(close['bench_runs']) and month in reported
    # The prompt asks for the closes in order: each first settled after
    # the one before it.
    firsts = [close['first_settled_at'] for close in closes.values()]
    in_order = None not in firsts and firsts == sorted(firsts)
    quick = [c for c, command, *_ in commands if 'make quick' in command]
    faults = [step_command_faults(command, unread) for _, command, unread, cut in commands if not cut]
    repeated = {}
    seen = set()
    for cursor, command, *_ in commands:
        if command in seen and first_cut is not None and cursor > first_cut:
            repeated[command] = repeated.get(command, 0) + 1
        seen.add(command)
    usage = [e['data'] for e in events if e['event'] == 'usage']
    summarizer = [u for u in usage if u.get('purpose') == 'compaction']
    work = [u for u in usage if u.get('purpose') != 'compaction']
    # The view each model call was made under: the summary version and its
    # cut, and the elision floor. A step or a move at the same head repeats
    # its version, so the cut and the floor tell those apart.
    view, views = {'compaction': None, 'cut': None, 'floor': 0}, []
    for event in events:
        if event['event'] == 'compacted':
            view = {**view, 'compaction': event['data']['version'], 'cut': event['data'].get('cut')}
        elif event['event'] == 'elided':
            view = {**view, 'floor': event['data']['through']}
        elif event['event'] == 'usage' and event['data'].get('purpose') != 'compaction':
            views.append(view)

    def total(rows, field):
        return sum(row.get(field) or 0 for row in rows)

    # Each summary installed, with the summarizer calls it took, what it
    # summarized, and how it was sent: a copy of the bot's call or a request
    # of its own, with the runtime's estimate of each. A binary from before
    # the event named its request reports none.
    summaries, spent = [], []
    for event in events:
        data = event['data']
        if event['event'] == 'usage' and data.get('purpose') == 'compaction':
            spent.append(data)
        elif event['event'] == 'compacted':
            request = data.get('request') or {}
            summaries.append({'catch_up': data.get('catch_up'), 'span_bytes': data.get('bytes'),
                              'view_bytes': (data.get('context_before') or {}).get('bytes'),
                              'limit_bytes': (data.get('input_limit') or {}).get('bytes'),
                              'form': request.get('form'), 'copied_items': request.get('items'),
                              'beside': request.get('beside'),
                              'estimate': request.get('estimate'),
                              'calls': len(spent), 'input_tokens': total(spent, 'input_tokens'),
                              'cached_input_tokens': total(spent, 'cached_input_tokens'),
                              'output_tokens': total(spent, 'output_tokens')})
            spent = []

    # A summary's latency: from its send to the send of the model call it
    # held back, which also counts recording the compaction. Attempts in a
    # row, such as a retry after one that failed, are one interval. A
    # summary's own call that ran beside the turn's calls says so; it held
    # the turn back only from when the turn began to wait for it, which its
    # compaction records, or the wait itself when it installed nothing, to
    # the next call, or for the wait alone when the turn parked or ended
    # next. One the turn ended on before another call held it back to the
    # turn's end, which `finished_ms` gives by turn.
    held, start, joined = 0, None, None
    for event in events:
        data = event['data']
        request = data.get('request') or {}
        if event['event'] == 'compacted' and request.get('beside') and request.get('waited_from_ms'):
            joined = request
        elif event['event'] == 'compaction_waited':
            joined = data
        elif event['event'] in ('turn_waiting', 'turn_paced', 'turn_finished') and joined:
            held += joined['waited_ms']
            joined = None
        if event['event'] == 'turn_finished' and start is not None:
            held += (finished_ms or {})[event['turn']] - start
            start = None
        if event['event'] != 'usage' or not data.get('sent_ms') or data.get('beside'):
            continue
        if data.get('purpose') == 'compaction':
            start = data['sent_ms'] if start is None else start
        elif data.get('purpose') is None:
            if start is not None:
                held += data['sent_ms'] - start
                start = None
            if joined:
                held += data['sent_ms'] - joined['waited_from_ms']
                joined = None

    return {
        'hidden_tests': f'{passed}/{cases}', 'hidden_failure': failure,
        # The sustained task is also its settlements.
        # A sustained bot is correct only once it has taken in the
        # correction.
        'correct': passed == cases and (not closes or corrected_at is not None) and all(
            c['settled_correctly'] and c['settled_after_correction'] is not False for c in closes.values()),
        'vendor_intact': vendor_intact,
        'make_quick_runs': count_lines(root / '.quick-attempts'),
        'make_quick_calls_after_first_compaction': sum(first_cut is not None and c > first_cut for c in quick),
        'migrations_applied': count_lines(root / '.migrations.log'),
        # The number counts only when the benchmark printed it, not when it
        # was read from where the benchmark keeps it; the sustained task
        # needs every close's.
        'reported_throughput': all(c['reported'] for c in closes.values()) if closes else bool(
            steps['bench_runs']) and reports(answer, facts['throughput']),
        **steps,
        'followed_workflow': steps['env_check_before_edits'] and (
            in_order and all(c['checked_before_settle'] and c['benched_after_settle'] for c in closes.values())
            if closes else steps['bench_after_check']),
        'closes': closes,
        'closes_settled_correctly': sum(c['settled_correctly'] for c in closes.values()),
        'closes_reported': sum(c['reported'] for c in closes.values()),
        'commands': [command[:160] for _, command, *_ in commands],
        # Step commands against the sustained prompt: output filtered, or
        # several steps in one command.
        'filtered_step_commands': sum(filtered for filtered, _ in faults),
        'combined_step_commands': sum(combined for _, combined in faults),
        # The parser cannot follow every shell form, so a command that names
        # a step in any form but the plain one is listed for a person, as is
        # every call too long for its event to hold.
        'step_commands_to_review': [('[cut] ' if cut else '') + command[:160] for _, command, _, cut in commands
                                    if closes and (cut or NAMES_STEP.search(command)
                                                   and not PLAIN_STEP.match(command))],
        'compactions': len(compactions),
        'elisions': sum(e['event'] == 'elided' for e in events),
        'repeated_commands_after_first_compaction': repeated,
        'retrieval_calls': sum(1 for c in calls.values() if c['name'] == 'history'
                               or (c['name'] == 'read' and 'result/' in (c.get('arguments') or ''))),
        'model_calls': len(work),
        # The largest context the task sent, as the provider counted it.
        'peak_input_tokens': max((row.get('input_tokens') or 0 for row in work), default=0),
        'views': views,
        'input_tokens': total(work, 'input_tokens'),
        'cached_input_tokens': total(work, 'cached_input_tokens'),
        'output_tokens': total(work, 'output_tokens'),
        'summarizer_calls': len(summarizer),
        'summarizer_input_tokens': total(summarizer, 'input_tokens'),
        'summarizer_cached_input_tokens': total(summarizer, 'cached_input_tokens'),
        'summarizer_output_tokens': total(summarizer, 'output_tokens'),
        'summarizer_ms': held,
        'summaries': summaries,
        # The whole task's input, model and summarizer, with cached tokens
        # at a tenth of the price.
        'input_token_equivalents': round(sum(
            total(rows, 'input_tokens') - total(rows, 'cached_input_tokens') * 0.9 for rows in (work, summarizer))),
    }


def run_condition(binary, spec, model, condition, trials, out_dir, env, seed, timeout, context_bytes=None):
    provider, family, url, key_env = spec
    size, budget, tools = CONDITIONS[condition]
    budget = context_bytes or budget
    root = Path(tempfile.mkdtemp(prefix=f'long-task-{condition}-', dir=out_dir))
    client = Client(binary, root / 'state.sqlite', url, tools=tools, model=model, key_env=key_env, env=env,
                    provider=provider, family=family, settings={'context_bytes': budget})
    results = {}
    try:
        names = [f'{condition}-{n}' for n in range(trials)]
        facts, turns = {}, {}
        for n, name in enumerate(names):
            work = root / name
            work.mkdir()
            facts[name] = workspace(work, seed + n, size)
            client.request('create', bot=name, workspace=str(work), instructions=INSTRUCTIONS,
                           tools=tools.split(','), compaction_instructions=COMPACTION)
        task = prompt(size)
        started, submitted = time.monotonic(), {}
        for name in names:
            submitted[name] = time.monotonic()
            turns[name] = client.request('submit', bot=name, request_id='task', prompt=task)['result']['turn']
        # Live events: count completed tools per bot, steer once each passes
        # the task's steer point, and collect the terminal events of tasks
        # and steers, with each task's time from its submission to its end
        # as the reader received it. The sustained task's point is in its
        # step record, written before the tool completes.
        done, steers, ends, completed, finished = {}, {}, {}, {name: 0 for name in names}, {}
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
                finished[name] = round(message['_received_at'] - submitted[name], 1)
                continue
            completed[name] += 1
            due = (settled_closes(root / name) >= SUSTAINED_STEER_CLOSES if size == 'sustained'
                   else completed[name] >= STEER_AFTER)
            if due and name not in steers:
                reply = client.request('submit', bot=name, request_id='correction', prompt=CORRECTION,
                                       delivery='steer', expected_turn=turns[name])
                steers[name] = reply.get('result') or {'error': reply.get('error')}
        wall = round(max(m['_received_at'] for m in done.values()) - started, 1)
        # A steer still queued when its task ends fails then, as it names
        # that task's turn.
        for name, steer in steers.items():
            if 'turn' in steer and name not in ends:
                ends[name] = client.finished(steer['turn'], timeout=60)['data']
        # Failed summaries are reported live only.
        failures = {name: [m.get('error') for m in client.saved
                           if m.get('event') == 'compaction_failed' and m.get('bot') == name] for name in names}
        # Where each sustained bot took in the correction: the steps it had
        # recorded by the time the daemon marked the steer's turn steered,
        # at a round boundary before the model's next call could run one.
        marks = {}
        for name, end in ends.items():
            if size == 'sustained' and end.get('status') == 'steered':
                steered = next(t for t in page_rows(client, 'turns', name) if t['turn'] == steers[name]['turn'])
                marks[name] = steps_before(root / name, steered['finished_ms'])
        for name in names:
            events = list(page_rows(client, 'events', name))
            finished_ms = {t['turn']: t['finished_ms'] for t in page_rows(client, 'turns', name)}
            checkpoint = done[name]['data'].get('checkpoint')
            answer = ''
            if checkpoint:
                item = node_item(client, name, checkpoint)['result']
                answer = ''.join(c.get('text', '') for c in item.get('content', []) if isinstance(c, dict))
            results[name] = {'status': done[name]['data']['status'], 'error': done[name]['data'].get('error'),
                             'wall_s': finished[name],
                             'steer': steer_outcome(steers.get(name), ends.get(name)),
                             'answer': answer[:2000], 'compaction_failures': failures[name],
                             **score(root / name, facts[name], events, answer, marks.get(name), finished_ms)}
        client.request('shutdown')
    finally:
        client.close(kill=True)
    return {'condition': condition, 'task': size, 'context_bytes': budget, 'tools': tools, 'wall_s': wall,
            'bots': results}


def steer_outcome(steer, end):
    """Whether the correction reached the task: its turn's final status."""
    if steer is None:
        return 'not sent: the task ended before its steer point'
    if 'turn' not in steer:
        return f"refused: {steer['error']}"
    return end['status'] if end['status'] == 'steered' else f"{end['status']}: {end.get('error')}"


def summarize(block):
    bots = list(block['bots'].values())

    def count(key):
        return f"{sum(1 for b in bots if b[key])}/{len(bots)}"

    return {'condition': block['condition'], 'task': block['task'], 'context_bytes': block['context_bytes'],
            'completed': f"{sum(b['status'] == 'completed' for b in bots)}/{len(bots)}",
            'steered': f"{sum(b['steer'] == 'steered' for b in bots)}/{len(bots)}",
            'correct': count('correct'), 'vendor_intact': count('vendor_intact'),
            'reported_throughput': count('reported_throughput'),
            'followed_workflow': count('followed_workflow'),
            'migrated_once': f"{sum(b['migrations_applied'] == 1 for b in bots)}/{len(bots)}",
            'make_quick_runs': [b['make_quick_runs'] for b in bots],
            'compactions': [b['compactions'] for b in bots],
            'peak_input_tokens': [b['peak_input_tokens'] for b in bots],
            'retrieval_calls': [b['retrieval_calls'] for b in bots],
            'tokens_in_cached_out': [sum(b['input_tokens'] for b in bots),
                                     sum(b['cached_input_tokens'] for b in bots),
                                     sum(b['output_tokens'] for b in bots)],
            'summarizer_in_cached_out': [sum(b['summarizer_input_tokens'] for b in bots),
                                         sum(b['summarizer_cached_input_tokens'] for b in bots),
                                         sum(b['summarizer_output_tokens'] for b in bots)],
            'summarizer_ms': sum(b['summarizer_ms'] for b in bots),
            # What a correct task cost, none when no task was correct, and
            # how long a bot took to finish.
            'input_token_equivalents_per_correct_task': round(
                sum(b['input_token_equivalents'] for b in bots) / correct) if (
                    correct := sum(b['correct'] for b in bots)) else None,
            'bot_wall_s': sorted(b['wall_s'] for b in bots),
            **({'closes_settled_correctly': [b['closes_settled_correctly'] for b in bots],
                'closes_reported': [b['closes_reported'] for b in bots],
                'settle_runs': [sum(c['settle_runs'] for c in b['closes'].values()) for b in bots],
                'filtered_step_commands': [b['filtered_step_commands'] for b in bots],
                'combined_step_commands': [b['combined_step_commands'] for b in bots],
                'step_commands_to_review': [len(b['step_commands_to_review']) for b in bots]}
               if block['task'] == 'sustained' else {}),
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
    parser.add_argument('--context-bytes', type=int, help="every condition's budget instead of its own")
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
                              args.trials, out_dir, env, args.seed, args.timeout, args.context_bytes)
        blocks.append(block)
        print(json.dumps(summarize(block)), flush=True)
        args.out.write_text(json.dumps({'binary_sha256': file_hash(args.binary), 'model': args.model,
                                        'seed': args.seed, 'conditions': blocks}, indent=1))


if __name__ == '__main__':
    main()
