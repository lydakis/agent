"""Does memory carry a decision from one task to a later one, and does a
later task catch a remembered fact the code now contradicts?

Each bot gets one task in a fresh git worktree of a small synthetic project,
`shop`, the way a coordinator's task starts. Every bot has its own HOME and
so its own daemon and its own `~/.agents/memory`, so no trial sees another's
saves. Bots are made as `agent run --new --agents` makes them, so their
instructions are what the composer gives today.

Conditions:

- `none`: no facts and no `memory` skill, the control.
- `memory`: the facts below saved with the app's `~/.agent/memory` script
  and the app's `memory` skill installed, both from this checkout, so a new agent's instructions
  carry both memory indexes (the person's and the project's).

Scenarios, each a fact the code does not show and a task that needs it:

- `decision`: the project decided API timestamps end in `Z`. The task adds
  `created_at` to `Order.to_json()`; Python's `isoformat()` gives `+00:00`.
- `preference`: the person wants error messages to start with a bracketed
  code. The task makes `add_item` reject a quantity below 1.
- `stale`: memory says prices round half to even with `round_price` in
  `shop/money.py`, but that file is gone and `shop/pricing.py` rounds half
  up. The task adds a discount to `Order.total()`, rounded as the project
  rounds. Correct means the code, not the fact, won; `fact_changed` and
  `fact_after` show whether and how the agent then fixed the fact, as the
  skill asks.

Scores come from hidden checks run in the worktree after the turn, not from
the agent's account. Each bot reports its turn's status, model rounds,
input, cached input and output tokens (the daemon's totals), shell commands
that touched memory, the facts it saved or removed, and its time.

Real model, real spend or plan quota. Run it on the ChatGPT plan with
Codex's login, naming the model as Codex's /model picker shows it:

    .local/venv/bin/python -m bench.memory_eval --model chatgpt/MODEL \\
        --out .local/memory-eval/MODEL.json

See docs/MEMORY_EVAL.md, which has the cost estimate. `--self-check`
scores reference answers and makes no model call.
"""
import argparse
import concurrent.futures
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.targets import clean_env, file_hash  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
BUILD = 'cargo build --release --locked -p agent-runtime -p agent-app'
PROJECT = 'shop'
CONDITIONS = ('none', 'memory')
# The key variable each built-in provider reads.
KEYS = {'openai': 'OPENAI_API_KEY', 'anthropic': 'ANTHROPIC_API_KEY', 'openrouter': 'OPENROUTER_API_KEY'}

FILES = {
    '.agents/project.toml': f'name = "{PROJECT}"\n',
    'AGENTS.md': '# shop\n\nRun `python3 -m unittest` from the repository root before you finish.\n',
    'shop/__init__.py': '',
    'shop/pricing.py': '''"""How shop rounds money."""
from decimal import ROUND_HALF_UP, Decimal


def quantize_price(value):
    """A price in whole cents, half a cent rounding up."""
    return Decimal(value).quantize(Decimal('0.01'), rounding=ROUND_HALF_UP)
''',
    'shop/orders.py': '''"""Orders."""
from decimal import Decimal

from shop.pricing import quantize_price


class Order:
    def __init__(self, id, created, items=()):
        self.id = id
        self.created = created  # an aware UTC datetime
        self.items = list(items)  # (sku, unit price as Decimal, quantity)

    def total(self):
        return quantize_price(sum((price * quantity for _, price, quantity in self.items), Decimal(0)))

    def to_json(self):
        return {'id': self.id,
                'items': [{'sku': sku, 'price': str(price), 'quantity': quantity}
                          for sku, price, quantity in self.items]}
''',
    'shop/cart.py': '''"""A cart is a dict from SKU to quantity."""


def add_item(cart, sku, quantity=1):
    cart[sku] = cart.get(sku, 0) + quantity
    return cart
''',
    'tests/__init__.py': '',
    'tests/test_shop.py': '''import unittest
from datetime import datetime, timezone
from decimal import Decimal

from shop.cart import add_item
from shop.orders import Order


class ShopTests(unittest.TestCase):
    def test_add_item_counts(self):
        self.assertEqual(add_item(add_item({}, 'a'), 'a', 2), {'a': 3})

    def test_total(self):
        order = Order(1, datetime(2026, 1, 2, tzinfo=timezone.utc), [('a', Decimal('1.25'), 2)])
        self.assertEqual(order.total(), Decimal('2.50'))

    def test_to_json_lists_items(self):
        order = Order(1, datetime(2026, 1, 2, tzinfo=timezone.utc), [('a', Decimal('1.25'), 2)])
        self.assertEqual(order.to_json()['items'], [{'sku': 'a', 'price': '1.25', 'quantity': 2}])


if __name__ == '__main__':
    unittest.main()
''',
}

# name: (scope, type, description, source, text)
FACTS = {
    'api-timestamps': (
        'project', 'project', 'API timestamps are UTC ISO-8601 ending in Z, never +00:00',
        'the person, 2026-09-30',
        'Timestamps in API output (`to_json`) are UTC ISO-8601 with a trailing `Z` and whole seconds, '
        'like `2026-09-30T12:00:00Z`, never `+00:00`.\n\n**Why:** the mobile client parses only that '
        'form. Decided with the person on 2026-09-30.'),
    'error-codes': (
        'user', 'feedback', 'Error messages start with a bracketed upper-case code',
        'the person, 2026-09-28',
        'Error messages start with a bracketed upper-case code, like '
        '`[E_NOT_FOUND] order 7 does not exist`.\n\n**Why:** the person searches logs by code.\n\n'
        '**How to apply:** every exception message and log line an agent writes.'),
    'price-rounding': (
        'project', 'reference', 'Prices round half to even with round_price in shop/money.py',
        'turn:lead/3',
        'Prices are rounded with `round_price()` in `shop/money.py`, which rounds half to even. '
        'Use it for any new price calculation.'),
}

SCENARIOS = {
    'decision': 'Add a `created_at` field to `Order.to_json()` in shop/orders.py, from the order\'s '
                '`created` time. Run the tests before you finish.',
    'preference': 'Make `add_item` in shop/cart.py reject a quantity below 1 with a ValueError. '
                  'Run the tests before you finish.',
    'stale': 'Give `Order.total()` in shop/orders.py an optional `discount` percentage (default 0), '
             'taken off before rounding, and round the result the way this project rounds prices. '
             'Run the tests before you finish.',
}

# Each hidden check prints one JSON object: `correct` and what it saw.
CHECKS = {
    'decision': '''
import json
from datetime import datetime, timezone
from shop.orders import Order
try:
    # Microseconds too, since the decision says whole seconds.
    got = Order(1, datetime(2026, 1, 2, 3, 4, 5, 678901, tzinfo=timezone.utc)).to_json().get('created_at')
except Exception as error:
    got = f'raised {error!r}'
print(json.dumps({'correct': got == '2026-01-02T03:04:05Z', 'got': str(got)}))
''',
    'preference': '''
import json, re
from shop.cart import add_item
messages = []
for quantity in (0, -1):
    try:
        add_item({}, 'a', quantity)
        messages.append(None)
    except ValueError as error:
        messages.append(str(error))
    except Exception as error:
        messages.append(f'raised {error!r}')
rejects = all(m is not None and not m.startswith('raised ') for m in messages)
coded = rejects and all(re.match(r'^\\[E_[A-Z0-9_]+\\] \\S', m) for m in messages)
try:
    accepts = add_item({}, 'a', 1) == {'a': 1}
except Exception:
    accepts = False
print(json.dumps({'correct': bool(coded and accepts), 'rejects': rejects, 'coded': bool(coded),
                  'accepts': accepts, 'messages': messages}))
''',
    'stale': '''
import json, os
from datetime import datetime, timezone
from decimal import Decimal
from shop.orders import Order
order = Order(1, datetime(2026, 1, 2, tzinfo=timezone.utc), [('a', Decimal('10.05'), 1)])
try:
    # 10.05 less 50% is 5.025: half up gives 5.03, half to even 5.02.
    got = order.total(discount=50)
    plain = order.total()
except Exception as error:
    got = plain = f'raised {error!r}'
money = os.path.exists('shop/money.py')
print(json.dumps({'correct': got == Decimal('5.03') and plain == Decimal('10.05') and not money,
                  'got': str(got), 'undiscounted': str(plain), 'money_py_created': money}))
''',
}

# A reference answer per scenario, right and wrong, for --self-check.
REFERENCE = {
    'decision': (("'items': [", "'created_at': self.created.strftime('%Y-%m-%dT%H:%M:%SZ'),\n                'items': ["),
                 ("'items': [", "'created_at': self.created.isoformat().replace('+00:00', 'Z'),\n"
                  "                'items': [")),
    'preference': (("    cart[sku]", "    if quantity < 1:\n        raise ValueError(f'[E_QUANTITY] quantity must be at "
                    "least 1, got {quantity}')\n    cart[sku]"),
                   ("    cart[sku]", "    if quantity < 1:\n        raise ValueError('quantity must be at least 1')\n"
                    "    cart[sku]")),
    'stale': (("    def total(self):\n        return quantize_price(sum((price * quantity for _, price, quantity in "
               "self.items), Decimal(0)))",
               "    def total(self, discount=0):\n        gross = sum((price * quantity for _, price, quantity in "
               "self.items), Decimal(0))\n        return quantize_price(gross * (100 - Decimal(discount)) / 100)"),
              ("    def total(self):\n        return quantize_price(sum((price * quantity for _, price, quantity in "
               "self.items), Decimal(0)))",
               "    def total(self, discount=0):\n        gross = sum((price * quantity for _, price, quantity in "
               "self.items), Decimal(0))\n        return round(gross * (100 - Decimal(discount)) / 100, 2)")),
}
SOURCES = {'decision': 'shop/orders.py', 'preference': 'shop/cart.py', 'stale': 'shop/orders.py'}


def git(cwd, *args):
    env = {**clean_env(), 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1',
           'GIT_AUTHOR_NAME': 'eval', 'GIT_AUTHOR_EMAIL': 'eval@localhost',
           'GIT_COMMITTER_NAME': 'eval', 'GIT_COMMITTER_EMAIL': 'eval@localhost'}
    subprocess.run(['git', '-C', str(cwd), *args], check=True, env=env, capture_output=True)


def project(root):
    """The project's main checkout at `root/shop` and a task's fresh
    worktree beside it, which is where the bot works."""
    main = root / PROJECT
    for path, text in FILES.items():
        (main / path).parent.mkdir(parents=True, exist_ok=True)
        (main / path).write_text(text)
    git(main, 'init', '-q', '-b', 'main')
    git(main, 'add', '-A')
    git(main, 'commit', '-q', '-m', 'shop')
    task = root / 'task'
    git(main, 'worktree', 'add', '-q', '-b', 'task', str(task))
    return task


def check(worktree, scenario):
    try:
        out = subprocess.run([sys.executable, '-I', '-c', f'import sys; sys.path.insert(0, ".")\n{CHECKS[scenario]}'],
                             cwd=worktree, env=clean_env(), capture_output=True, text=True, timeout=60)
    except subprocess.TimeoutExpired:
        return {'correct': False, 'error': 'timeout'}
    try:
        return json.loads(out.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        return {'correct': False, 'error': (out.stderr or out.stdout)[-2000:]}


def visible_tests(worktree):
    try:
        out = subprocess.run([sys.executable, '-m', 'unittest', '-q'], cwd=worktree, env=clean_env(),
                             capture_output=True, text=True, timeout=120)
    except subprocess.TimeoutExpired:
        return False
    return out.returncode == 0


def provider_keys(specs):
    """The key variables the daemon's providers read, which it registers as
    credentials and so keeps out of the bot's shell: a spec's KEY_ENV, else
    its built-in provider's own when it keeps the built-in URL. No other key
    is passed on."""
    keys = []
    for spec in specs.split():
        name, _, rest = spec.partition('=')
        fields = rest.split(',') if rest else []
        if len(fields) > 2 and fields[2]:
            keys.append(fields[2])
        elif not (len(fields) > 1 and fields[1]):
            # A custom URL with no key field sends no credential.
            keys.append(KEYS.get(name))
    return [key for key in keys if key]


def home(root, condition, app):
    """A HOME of the bot's own: the memory script, and for `memory` the
    skill and the facts, saved through the script."""
    home = root / 'home'
    (home / '.agent').mkdir(parents=True)
    script = home / '.agent/memory'
    quoted = "'" + str(app).replace("'", "'\\''") + "'"
    script.write_text(f'#!/bin/sh\nexec {quoted} --memory "$@"\n')
    script.chmod(0o755)
    if condition == 'memory':
        shutil.copytree(ROOT / 'app/skills/memory', home / '.agents/skills/memory')
        for name, (scope, kind, description, source, text) in FACTS.items():
            where = ['--user'] if scope == 'user' else ['--project', PROJECT]
            subprocess.run([str(script), 'save', name, '--type', kind, '--description', description,
                            '--source', source, *where, '--', text],
                           check=True, capture_output=True, env={**clean_env(), 'HOME': str(home)})
    return home


def memory_files(home):
    root = home / '.agents/memory'
    return {str(p.relative_to(root)): p.read_text() for p in sorted(root.rglob('*.md'))
            if p.name != 'MEMORY.md' and '.git' not in p.parts} if root.exists() else {}


def events(lines):
    found = []
    for line in lines.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if isinstance(event, dict) and 'event' in event:
            found.append(event)
    return found


def shell_commands(found):
    commands = []
    for event in found:
        data = event.get('data') or {}
        if event['event'] == 'tool_started' and data.get('name') == 'shell':
            try:
                commands.append(json.loads(data.get('arguments') or '{}').get('command', ''))
            except (ValueError, AttributeError):
                commands.append(data.get('arguments') or '')
    return commands


def answer(found):
    """The text the model streamed after its last tool call: its final
    reply. `run` streams text live only, as `text_delta`."""
    texts = []
    for event in found:
        if event['event'] == 'tool_started':
            texts = []
        elif event['event'] == 'text_delta':
            texts.append(event.get('text', ''))
    return ''.join(texts)[:2000]


def run_bot(binary, app, model, env, out_dir, condition, scenario, trial, budget, timeout):
    """One task in its own HOME and daemon, scored."""
    root = Path(tempfile.mkdtemp(prefix=f'{condition}-{scenario}-{trial}-', dir=out_dir))
    worktree = project(root)
    bot_home = home(root, condition, app)
    before = memory_files(bot_home)
    bot_env = {**env, 'HOME': str(bot_home)}
    name = f'{scenario}-{trial}'
    agent = [str(binary)]
    started = time.monotonic()
    try:
        run = subprocess.run([*agent, 'run', '--new', '--agents', '--bot', name, '--workspace', str(worktree),
                              '--model', model, '--turn-budget-tokens', str(budget), '--', SCENARIOS[scenario]],
                             cwd=worktree, env=bot_env, capture_output=True, text=True, timeout=timeout)
        stream, error = run.stdout, run.stderr[-2000:]
    except subprocess.TimeoutExpired as expired:
        # What streamed before the timeout, bytes on some Pythons.
        out = expired.stdout or ''
        stream, error = out.decode() if isinstance(out, bytes) else out, 'timeout'
        subprocess.run([*agent, 'interrupt', '--bot', name], env=bot_env, capture_output=True, timeout=30)
    wall = round(time.monotonic() - started, 1)
    # A finished turn returns at once; an interrupted one once it settles.
    view = subprocess.run([*agent, 'wait', '--timeout', '60s', f'turn:{name}/1'], env=bot_env,
                          capture_output=True, text=True, timeout=90)
    try:
        turn = json.loads(view.stdout)['results'][f'turn:{name}/1']
    except (ValueError, KeyError, TypeError):
        turn = {'error': view.stderr[-500:]}
    subprocess.run([*agent, 'shutdown'], env=bot_env, capture_output=True, timeout=60)
    found = events(stream)
    commands = shell_commands(found)
    after = memory_files(bot_home)
    stale = 'projects/shop/price-rounding.md'
    return {
        'condition': condition, 'scenario': scenario, 'trial': trial,
        'status': turn.get('status'), 'error': turn.get('error') or (error if not found else None),
        **{key: turn.get(key) for key in ('model_rounds', 'input_tokens', 'cached_input_tokens', 'output_tokens')},
        'wall_s': wall,
        'check': check(worktree, scenario), 'visible_tests_pass': visible_tests(worktree),
        'memory_commands': [c for c in commands if '.agent/memory' in c or '.agents/memory' in c],
        'shell_calls': len(commands),
        'memory_saved': sorted(k for k in after if before.get(k) != after[k]),
        'memory_removed': sorted(k for k in before if k not in after),
        # Whether the agent fixed the wrong fact is read from its text.
        **({'fact_changed': after.get(stale) != before[stale], 'fact_after': after.get(stale)}
           if condition == 'memory' and scenario == 'stale' else {}),
        'answer': answer(found),
        'dir': str(root),
    }


def summarize(rows):
    blocks = {}
    for row in rows:
        blocks.setdefault((row['condition'], row['scenario']), []).append(row)
    summary = []
    for (condition, scenario), block in sorted(blocks.items()):
        total = lambda key: sum(r[key] or 0 for r in block)  # noqa: E731
        summary.append({'condition': condition, 'scenario': scenario, 'trials': len(block),
                        'correct': sum(bool(r['check'].get('correct')) for r in block),
                        'completed': sum(r['status'] == 'completed' for r in block),
                        'model_rounds': [r['model_rounds'] for r in block],
                        'tokens_in_cached_out': [total('input_tokens'), total('cached_input_tokens'),
                                                 total('output_tokens')],
                        'memory_commands': [len(r['memory_commands']) for r in block],
                        **({'fact_changed': sum(r['fact_changed'] for r in block)}
                           if 'fact_changed' in block[0] else {})})
    return summary


def self_check():
    """Score each scenario's right and wrong reference answer, with no
    model: the right one must pass, the wrong one fail, the untouched
    project fail, and the visible tests pass."""
    with tempfile.TemporaryDirectory() as temp:
        for scenario, answers in REFERENCE.items():
            for label, (old, new), want in (('right', answers[0], True), ('wrong', answers[1], False)):
                worktree = project(Path(temp) / f'{scenario}-{label}')
                assert not check(worktree, scenario)['correct'], (scenario, 'untouched')
                source = worktree / SOURCES[scenario]
                text = source.read_text()
                assert old in text, (scenario, label)
                source.write_text(text.replace(old, new, 1))
                got = check(worktree, scenario)
                assert got['correct'] is want, (scenario, label, got)
                assert visible_tests(worktree), (scenario, label)
    print(json.dumps({'self_check': 'ok', 'scenarios': list(REFERENCE)}))


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--model', help='PROVIDER/MODEL: chatgpt/MODEL on the plan')
    parser.add_argument('--out', type=Path)
    parser.add_argument('--trials', type=int, default=3, help='bots per condition and scenario, all at once')
    parser.add_argument('--conditions', nargs='+', default=list(CONDITIONS), choices=CONDITIONS)
    parser.add_argument('--scenarios', nargs='+', default=list(SCENARIOS), choices=list(SCENARIOS))
    parser.add_argument('--binary', type=Path, default=ROOT / '.local/target/release/agent')
    parser.add_argument('--memory-app', type=Path, default=ROOT / '.local/target/release/agent-app',
                        help='agent-app built from this checkout, so the memory script matches its skill')
    parser.add_argument('--provider', help='AGENT_PROVIDER for the daemons (default: the model\'s provider)')
    parser.add_argument('--turn-budget-tokens', type=int, default=400_000, help='cap on each task\'s turn')
    parser.add_argument('--timeout', type=float, default=1200, help='seconds each task may take')
    parser.add_argument('--self-check', action='store_true', help='score reference answers; no model call')
    args = parser.parse_args()
    if args.self_check:
        return self_check()
    if not (args.model and args.out):
        parser.error('--model and --out are required')
    for built in (args.binary, args.memory_app):
        if not built.exists():
            parser.error(f'{built} is missing: {BUILD}')
    app = args.memory_app.resolve()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    env = clean_env()
    env['AGENT_PROVIDER'] = args.provider or args.model.split('/', 1)[0]
    env.update({k: os.environ[k] for k in provider_keys(env['AGENT_PROVIDER']) if k in os.environ})
    # The daemon reads Codex's ChatGPT login from CODEX_HOME, which must
    # outlive the bot's own HOME.
    env['CODEX_HOME'] = os.environ.get('CODEX_HOME') or str(Path.home() / '.codex')
    # Bots work outside this checkout, so composing their instructions
    # finds the shop's AGENTS.md and not this repository's.
    out_dir = Path(tempfile.mkdtemp(prefix='agent-memory-eval-'))
    if ROOT in out_dir.resolve().parents:
        parser.error(f'{out_dir} is inside {ROOT}: set TMPDIR elsewhere')
    observed = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
    jobs = [(c, s, t) for c in args.conditions for s in args.scenarios for t in range(args.trials)]
    with concurrent.futures.ThreadPoolExecutor(len(jobs)) as pool:
        rows = list(pool.map(lambda job: run_bot(args.binary.resolve(), app, args.model, env, out_dir, *job,
                                                 args.turn_budget_tokens, args.timeout), jobs))
    revision = subprocess.run(['git', '-C', str(ROOT), 'describe', '--always', '--dirty', '--abbrev=40'],
                              capture_output=True, text=True).stdout.strip()
    result = {'observed': observed, 'revision': revision, 'model': args.model,
              'binary_sha256': file_hash(args.binary), 'memory_app_sha256': file_hash(app),
              'bots_dir': str(out_dir),
              'summary': summarize(rows), 'bots': rows}
    args.out.write_text(json.dumps(result, indent=1))
    print(json.dumps(result['summary'], indent=1))
    print(f'bots kept in {out_dir}', file=sys.stderr)


if __name__ == '__main__':
    main()
