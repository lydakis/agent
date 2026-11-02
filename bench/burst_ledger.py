"""The burst's ledger: what became of each planted ask, from the run's own
evidence, so a score never rests on the lead's word that all is handled.

For each of the twenty asks it records when it was released, whether a
note brought it to the lead (`reached`), whether the lead sent it to a
task (`delegated`, from the lead's calls), and how it ended: a code ask's
change checked on main and on every branch tip (`finished`), a quick look
or the person's call found in what the lead said to the person (`said`).
Beside it go what every agent's turns cost and how they ended, recorded
while the run went (`events.jsonl`), so a task deleted before the end
still counts, the lookout's events not kept, and the run's processes'
peak memory and CPU.

The checks are evidence for the scorer, not the score: docs/ROLE_EVAL.md
still has the rows read from the transcript.
"""
import json
import re
import subprocess
import tempfile
import threading
import time
from pathlib import Path

# What a lead's call or words say of each ask, and how a code ask is
# checked: Python run at the root of a checkout, which passes by exiting 0.
LEDGER = {
    'B1 code ask: Order __str__': {
        'key': r"__str__|__repr__|19\.99 EUR|print as",
        'check': "from shop.orders import sample\nassert str(sample()[0]) == '#1 19.99 EUR'"},
    'B1 code ask: audit stamp in UTC': {
        'key': r"utcnow|audit",
        'check': "import warnings\nwarnings.simplefilter('error')\nfrom datetime import datetime\n"
                 "from shop.audit import stamp\nd = datetime.fromisoformat(stamp().replace('Z', '+00:00'))\n"
                 "assert d.utcoffset() is not None and d.utcoffset().total_seconds() == 0"},
    'B1 code ask: Order.is_refund': {
        'key': r"is_refund|refund",
        'check': "from datetime import datetime, timezone\nfrom decimal import Decimal\n"
                 "from shop.orders import Order\n"
                 "def refund(total):\n    o = Order(3, Decimal(total), 'EUR', datetime.now(timezone.utc))\n"
                 "    r = o.is_refund\n    return r() if callable(r) else r\n"
                 "assert refund('-1') is True and refund('1') is False"},
    'B1 code ask: session timeout': {
        'key': r"five hours|5 \* 60|sign(ed)?[ -]out|TIMEOUT|session",
        'check': "from datetime import datetime, timedelta\nfrom shop.session import is_expired\n"
                 "s = datetime(2026, 1, 1)\n"
                 "assert not is_expired(s, s + timedelta(hours=4)) and is_expired(s, s + timedelta(hours=6))"},
    'B1 code ask: format_money': {
        'key': r"format_money|money\.py",
        'check': "from decimal import Decimal\nfrom shop.money import format_money\n"
                 "assert format_money(Decimal('19.99'), 'EUR') == '19.99 EUR'"},
    'B1 code ask: tz test': {
        'key': r"timezone.aware|tz.?aware|aware|tzinfo",
        'check': "import pathlib, re\n"
                 "t = ''.join(p.read_text() for p in pathlib.Path('tests').glob('*.py'))\n"
                 "assert re.search(r'tzinfo|utcoffset', t) and 'sample' in t"},
    'B1 code ask: CSV export': {
        'key': r"CSV|csv|export",
        'check': "import io, inspect\nimport shop.export as e\nfrom shop.orders import sample\nok = False\n"
                 "for name, f in vars(e).items():\n"
                 "    if name.startswith('_') or not inspect.isfunction(f) or f.__module__ != e.__name__:\n"
                 "        continue\n"
                 "    for args in ((sample(),), (sample(), io.StringIO())):\n"
                 "        try:\n            r = f(*args)\n        except Exception:\n            continue\n"
                 "        text = r if isinstance(r, str) else (args[1].getvalue() if len(args) > 1 else '')\n"
                 "        ok = ok or 'id,total,created_at' in text.replace(' ', '')\n"
                 "assert ok"},
    'B1 code ask: README': {
        'key': r"README|readme",
        'check': "import pathlib\nfound = [p for p in pathlib.Path('.').iterdir() if p.name.lower().startswith('readme')]\n"
                 "assert found and 'test' in found[0].read_text().lower()"},
    'B1 code ask: gitignore': {
        'key': r"gitignore|__pycache__",
        'check': "assert '__pycache__' in open('.gitignore').read()"},
    'B1 code ask: now_utc': {
        'key': r"now_utc|clock\.py",
        'check': "from shop.clock import now_utc\nassert now_utc().utcoffset().total_seconds() == 0"},
    'B1 code ask: by_currency': {
        'key': r"by_currency",
        'check': "from shop.orders import by_currency, sample\nassert set(by_currency(sample())) == {'EUR', 'USD'}"},
    'B1 code ask: expired test': {
        'key': r"expired",
        'check': "import re\nt = open('tests/test_session.py').read()\n"
                 "assert re.search(r'assertTrue\\(\\s*is_expired', t)"},
    'B3 quick look: USD': {'said': r"\bUSD\b"},
    'B3 quick look: three': {'said': r"\b(three|3)\s+(unit\s+)?tests?\b"},
    'B3 quick look: shop/audit.py': {'said': r"audit\.py"},
    'B3 quick look: none in the repo': {'said': r"\b(no|none|isn.t|not|nothing)\b[^.\n]{0,60}\bCI\b|\bCI\b[^.\n]{0,60}\b(no|none|isn.t|not|nothing)\b"},
    'B3 quick look: the TIMEOUT constant': {'said': r"TIMEOUT"},
    "B4 the person's call: refunds in the export": {'said': r"refund[^.\n]{0,80}export|export[^.\n]{0,80}refund"},
    "B4 the person's call: promise the fix": {'said': r"(promise|ship|tell customers)[^.\n]{0,80}(week|fix|sign)"},
    "B4 the person's call: drop 3.9": {'said': r"3\.9"},
}

# A lead's call that hands work to another agent: a task started, or a
# message sent to one, by a tool or by `agent run` in a shell.
DELEGATES = re.compile(r'^(start_task|\S*(message|send|steer|tell)\S*) |agent"? run|AGENT_BIN\S* run')
# The calls that change files: a lead's own edits.
EDITS = {'write', 'edit', 'apply_patch'}

# The event kinds kept from every agent's stream, small ones only.
KINDS = {'created', 'forked', 'deleted', 'queued', 'accepted', 'usage', 'turn_finished'}


class Recorder:
    """Every agent's turns, usage and ends as they come (`agent follow
    --all`), written to `path`, so evidence outlives a task deleted later."""

    def __init__(self, agent, env, path):
        self.file = open(path, 'w')
        self.process = subprocess.Popen([str(agent), 'follow', '--all'], env=env, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, text=True)
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()

    def read(self):
        for line in self.process.stdout:
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get('event') in KINDS:
                self.file.write(json.dumps({'t': time.time(), **event}) + '\n')
                self.file.flush()

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
        self.thread.join(timeout=5)
        self.file.close()


class Sampler:
    """The run's processes, every few seconds: those whose command line
    names its HOME (the daemon, its companions), the ones `roots` names
    (the lead's `sonny acp`), and every process under them (their tools).
    The peak of their summed memory and the CPU time they had used."""

    def __init__(self, home, every=5, roots=lambda: ()):
        self.home, self.every, self.roots = str(home), every, roots
        self.peak_rss_kb, self.cpu_s = 0, {}
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def sample(self):
        out = subprocess.run(['ps', '-axo', 'pid=,ppid=,rss=,time=,command='], capture_output=True,
                             text=True).stdout
        rows, children = {}, {}
        for line in out.splitlines():
            parts = line.split(None, 4)
            if len(parts) < 5:
                continue
            rows[parts[0]] = parts
            children.setdefault(parts[1], []).append(parts[0])
        ours = [pid for pid, parts in rows.items() if self.home in parts[4]]
        ours += [str(pid) for pid in self.roots() if str(pid) in rows]
        seen, rss = set(), 0
        while ours:
            pid = ours.pop()
            if pid in seen:
                continue
            seen.add(pid)
            rss += int(rows[pid][2])
            self.cpu_s[pid] = seconds(rows[pid][3])
            ours += children.get(pid, [])
        self.peak_rss_kb = max(self.peak_rss_kb, rss)

    def run(self):
        while not self.stop.wait(self.every):
            self.sample()

    def close(self):
        self.sample()
        self.stop.set()
        self.thread.join(timeout=10)
        return {'peak_rss_mb': round(self.peak_rss_kb / 1024, 1), 'cpu_s': round(sum(self.cpu_s.values()), 1)}


def seconds(text):
    """`ps` time ([[DD-]HH:]MM:SS[.ss]) in seconds."""
    days, _, clock = text.rpartition('-')
    total = 0.0
    for part in clock.split(':'):
        total = total * 60 + float(part)
    return total + (int(days) * 86400 if days else 0)


def lead_view(acp_log):
    """From `acp.jsonl`: the notes the lead got, its calls and its words to
    the person, each with when."""
    notes, calls, said, chunks = [], [], [], {}
    for line in open(acp_log):
        row = json.loads(line)
        m = row.get('m') or {}
        if m.get('method') != 'session/update':
            continue
        u = m['params']['update']
        kind, t = u.get('sessionUpdate'), row['t']
        if kind == 'user_message':
            notes.append((t, ''.join(c.get('text', '') for c in u.get('content', []))))
        elif kind == 'tool_call':
            calls.append((t, u.get('name') or u.get('title') or '', json.dumps(u.get('rawInput'))))
        elif kind == 'agent_message_chunk':
            mid = u.get('messageId', '')
            if mid not in chunks:
                chunks[mid] = [t, '']
                said.append(chunks[mid])
            chunks[mid][0] = t
            chunks[mid][1] += u['content'].get('text', '')
    return notes, calls, [tuple(s) for s in said]


# What a command over the run's files gets of the run's environment: no
# provider key.
PASSED = ('HOME', 'PATH', 'LANG', 'TERM', 'GIT_CONFIG_GLOBAL', 'TMPDIR')


def confined(agent, env, cmd, cwd, writable=None, **kw):
    """Run `cmd` over files agents wrote as the agents' own commands run:
    in `agent sandbox`, network off, writing only `writable`, with the run's
    HOME and no key. git runs a repository's hooks and the programs its
    config names, and the code is the agents', so none of it runs as the
    person."""
    sandbox = [str(agent), 'sandbox', *(['--writable', str(writable)] if writable else []), '--', *cmd]
    return subprocess.run(sandbox, cwd=cwd, env={k: env[k] for k in PASSED if k in env}, capture_output=True,
                          **kw)


def tree_of(agent, repo, env, ref, tree):
    """`ref`'s files in `tree`, or False."""
    archive = confined(agent, env, ['git', 'archive', ref], repo)
    if archive.returncode != 0:
        return False
    return subprocess.run(['tar', '-x', '-C', tree], input=archive.stdout, capture_output=True).returncode == 0


def checked(agent, repo, env, ref, code):
    """Whether `code` passes at the root of `ref`'s files."""
    with tempfile.TemporaryDirectory() as tree:
        if not tree_of(agent, repo, env, ref, tree):
            return False
        run = confined(agent, env, ['python3', '-I', '-c', f'import sys; sys.path.insert(0, ".")\n{code}'], tree,
                       tree, timeout=60)
        return run.returncode == 0


def suite_passes(agent, repo, env, ref):
    with tempfile.TemporaryDirectory() as tree:
        if not tree_of(agent, repo, env, ref, tree):
            return False
        return confined(agent, env, ['python3', '-m', 'unittest', '-q'], tree, tree, timeout=300).returncode == 0


def agents(events_path):
    """Each agent's turns by how they ended, its tokens, and the errors of
    those that failed, from the recorded stream."""
    found = {}
    if not Path(events_path).exists():
        return found
    for line in open(events_path):
        e = json.loads(line)
        bot = e.get('bot')
        if not bot:
            continue
        a = found.setdefault(bot, {'turns': {}, 'input_tokens': 0, 'cached_input_tokens': 0,
                                   'output_tokens': 0, 'errors': [], 'deleted': False})
        data = e.get('data') or {}
        if e['event'] == 'turn_finished':
            status = data.get('status', '?')
            a['turns'][status] = a['turns'].get(status, 0) + 1
            if data.get('error'):
                a['errors'].append(data['error'])
        elif e['event'] == 'usage':
            for k in ('input_tokens', 'cached_input_tokens', 'output_tokens'):
                a[k] += data.get(k) or 0
        elif e['event'] == 'deleted':
            a['deleted'] = True
    return found


def unkept(lookout_log, lead):
    """The lookout's events from sources it did not keep for `lead`, and the
    most it said were waiting after a note."""
    dropped, waiting = 0, 0
    if Path(lookout_log).exists():
        for line in open(lookout_log):
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if row.get('lead') != lead:
                continue
            dropped += 'skipped' in row and 'source' in row
            waiting = max(waiting, row.get('waiting') or 0)
    return {'not_kept': dropped, 'most_waiting': waiting}


def ledger(out, agent, repo, env, events, timeline, lookout_log, resources=None, lead='shop.lead', ended=None):
    """Write `asks.json` and `asks.md` in `out`: each planted ask's way.
    An ask not acted on waits until `ended`, when the run ended; written
    again later, the ledger keeps the end it wrote first, or else takes the
    run's last evidence."""
    out = Path(out)
    notes, calls, said = lead_view(out / 'acp.jsonl')
    branches = confined(agent, env, ['git', 'for-each-ref', '--format=%(refname:short)', 'refs/heads'],
                        repo).stdout.decode().split()
    if ended is None and (out / 'asks.json').exists():
        ended = json.loads((out / 'asks.json').read_text())['summary'].get('ended')
    end = ended or max([t for t, _ in notes + said] + [t for t, _, _ in calls] + [s.get('t', 0) for s in timeline] + [0])
    rows = []
    for event in events:
        plant = event.get('plant') or ''
        if plant not in LEDGER:
            continue
        spec = LEDGER[plant]
        released = timeline[event['step'] - 1]['t'] if event['step'] - 1 < len(timeline) else None
        after = released or 0
        quoted = json.dumps(event['text'])[1:61]
        reached = next((t for t, text in notes if t >= after and quoted in text), None)
        row = {'ask': plant, 'from': event['from'], 'released': released,
               'reached_s': reached and round(reached - after)}
        handled = None
        if 'check' in spec:
            key = re.compile(spec['key'])
            delegated = next((t for t, name, text in calls
                              if t >= after and DELEGATES.search(f'{name} {text}') and key.search(text)), None)
            itself = next((t for t, name, text in calls
                           if t >= after and name in EDITS and key.search(text)), None)
            row['delegated_s'] = delegated and round(delegated - after)
            row['edited_itself_s'] = itself and round(itself - after)
            row['on_main'] = checked(agent, repo, env, 'main', spec['check'])
            row['on_branches'] = [b for b in branches if b != 'main' and checked(agent, repo, env, b, spec['check'])]
            # Acted on when sent to a task or when the lead began it itself.
            handled = min((t for t in (delegated, itself) if t is not None), default=None)
        else:
            words = re.compile(spec['said'], re.IGNORECASE)
            told = next((t for t, text in said if t >= after and words.search(text)), None)
            row['said_s'] = told and round(told - after)
            handled = told
        # How long it waited for the lead to act on it, or still waits.
        row['pending_s'] = round((end if handled is None else handled) - after) if released is not None else None
        row['handled'] = handled is not None
        rows.append(row)
    found = agents(events_path=out / 'events.jsonl')
    summary = {
        'asks': len(rows),
        'reached': sum(r['reached_s'] is not None for r in rows),
        'handled': sum(r['handled'] for r in rows),
        'code_on_main': sum(bool(r.get('on_main')) for r in rows),
        'code_on_a_branch_only': sum(bool(r.get('on_branches')) and not r.get('on_main') for r in rows),
        'oldest_pending_s': max((r['pending_s'] for r in rows if r['pending_s'] is not None), default=None),
        'main_tests_pass': suite_passes(agent, repo, env, 'main'),
        'agents': len(found),
        'turns_failed': sum(a['turns'].get('failed', 0) for a in found.values()),
        'deleted': sum(a['deleted'] for a in found.values()),
        'input_tokens': sum(a['input_tokens'] for a in found.values()),
        'cached_input_tokens': sum(a['cached_input_tokens'] for a in found.values()),
        'output_tokens': sum(a['output_tokens'] for a in found.values()),
        **unkept(lookout_log, lead),
        **(resources or {}),
        'ended': end,
    }
    (out / 'asks.json').write_text(json.dumps({'summary': summary, 'asks': rows, 'agents': found}, indent=1))
    lines = ['# The burst\'s asks', '',
             '| Ask | Reached (s) | Delegated (s) | Edited itself (s) | On main | On a branch | Said (s) | Waited (s) |',
             '| --- | --- | --- | --- | --- | --- | --- | --- |']
    dash = lambda v: '-' if v is None or v == [] else (', '.join(v) if isinstance(v, list) else str(v))
    for r in rows:
        on_main = None if 'on_main' not in r else ('yes' if r['on_main'] else 'no')
        lines.append(f"| {r['ask']} | {dash(r['reached_s'])} | {dash(r.get('delegated_s'))} | "
                     f"{dash(r.get('edited_itself_s'))} | {dash(on_main)} | "
                     f"{dash(r.get('on_branches'))} | {dash(r.get('said_s'))} | {dash(r['pending_s'])} |")
    lines += ['', '```', json.dumps(summary, indent=1), '```', '']
    (out / 'asks.md').write_text('\n'.join(lines))
    return summary
