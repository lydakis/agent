"""How a lead acts on its lookout's notes: today's, or every task message.

Usage:
  python3 -m bench.lookout_turns run REPLAY.jsonl --socket SOCK --out TURNS.jsonl
      [--bands OLD_REPLAY.jsonl [--band-judge SPEC]] [--arms today,every]
      [--labels LABELS.jsonl [--old OLD_REPLAY.jsonl]] [--jobs 4]
  python3 -m bench.lookout_turns grade TURNS.jsonl LABELS.jsonl --socket SOCK
      --grader PROVIDER/MODEL --out GRADES.jsonl [--old OLD_REPLAY.jsonl] [--jobs 4]
  python3 -m bench.lookout_turns score GRADES.jsonl

`run` reads a `sonny lookout --replay` of the store (any judge: only the
events, where the lead stood, and who asked for each turn are used) and,
for each event and arm, forks the lead where it stood, with only its wait
tool, and gives it that event's note as a dry run: it writes each tool call
it would make as a `WOULD:` line, then what it would reply to the person.
`today` is the note as it was before every message reached the lead: turn
ends as task updates, and a message written in a task only when the judge
picked it, by the bands of the replay `--bands` holds (keyed by task, turn
and kind; one judge's, `--band-judge`, when it holds several). An event that
note would not have told the lead of is written as unsent: the lead did
nothing. `every` is the note that tells the lead every message, the
person's quoted. Nothing a fork does reaches a task or the person.

`grade` has a model grade each turn against the hand label for its event:
whether the lead did what the label says it should, did something nothing
called for, and whether its reply to the person was silent, filler or news.
Labels are keyed by task, turn and kind, or by `n` of the replay `--old`
was, which then maps them.
"""
import argparse
import json
import socket
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

PERSON = 'the person'
QUOTE_CHARS = 2000
TURN_SECONDS = 300


class Daemon:
    """One connection to the daemon, a request at a time."""

    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(path)
        self.file = self.sock.makefile('rw', encoding='utf-8')
        ready = json.loads(self.file.readline())
        assert ready.get('event') == 'ready', ready
        self.next = 0

    def request(self, op, **params):
        self.next += 1
        self.file.write(json.dumps({'id': self.next, 'op': op, **params}) + '\n')
        self.file.flush()
        while True:
            message = json.loads(self.file.readline())
            if message.get('id') != self.next:
                continue
            if 'error' in message:
                raise Failed(message['error'], message.get('detail'))
            return message.get('result')

    def close(self):
        self.sock.close()


class Failed(Exception):
    def __init__(self, code, detail=None):
        super().__init__(f'{code}: {detail}')
        self.code = code


def key(row):
    """One event as one subscriber saw it: two leads watching a task each
    get their own rehearsal."""
    return (row.get('lead'), row['task'], row['turn'], row['kind'])


def event(k):
    """The event alone, as labels name it."""
    return k[1:]


def cut(text, most):
    if len(text) <= most:
        return text
    half = most // 2
    return f'{text[:half]}\n[... {len(text) - 2 * half} characters left out ...]\n{text[-half:]}'


def words(text):
    return ' '.join(text.split())


def ended_line(row):
    """A turn's end as the task updates say it."""
    status = (row.get('status') or 'ended').split(' (')[0].split(', after ')[0]
    by = row.get('asked_by') or PERSON
    task = row['task']
    return f'- {task}: turn:{task}/{row["turn"]} {status}, asked by {by}', by == PERSON


ACTIONS = {
    'redirects': 'pass it to every task it applies to',
    'conflict': 'pass it to the task it matters to, or reconcile the two tasks',
    'drifts': 'settle what the person means and steer the task with that reading',
    'stuck': 'unblock it, or ask the person for what it needs',
    'result': 'tell the person',
}


def today(row, band):
    """The note before every message reached the lead, for one event; None
    when it sent none."""
    task, turn = row['task'], row['turn']
    handle = f'turn:{task}/{turn}'
    lines = ['Task updates: what your tasks did since you last heard; the wait tool reads each turn by its handle.']
    name = (band or {}).get('band', 'Drop')
    reasons = (band or {}).get('reasons') or []
    woke = name.startswith('Wake')
    why = ', '.join(f'{id.replace("_", " ")} {p:.2f}' for id, p in reasons) or 'not checked'
    if row['kind'] == 'Said' and not woke and name != 'Hold':
        return None
    if woke:
        todo = []
        for id, _ in reasons:
            action = ACTIONS.get(id, 'look')
            passes = action.startswith('pass it') and any(t.startswith('pass it') for t in todo)
            if action not in todo and not passes:
                todo.append(action)
        what = (f'{row.get("from") or "someone"} wrote in it' if row['kind'] == 'Said'
                else f'turn {row.get("status") or "ended"}')
        quote = f': "{cut(words(row["words"]), QUOTE_CHARS)}"' if row.get('from') == PERSON and row.get('words') else ''
        lines.append('May need you:')
        lines.append(f'- {task}: {handle}, {what} ({why}; {", and ".join(todo) or "look"}){quote}')
    if row['kind'] == 'Ended' and not woke:
        line, theirs = ended_line(row)
        lines.append('The person asked for these in the task themselves, so they are theirs:' if theirs
                     else 'Ended, waiting for an approval, or gone quiet:')
        lines.append(line)
    if name == 'Hold' and row['kind'] == 'Said':
        lines.append(f'Worth a look if you are there anyway: {handle} ({why}).')
    lines.append('A quoted message is the person\'s own words. Do what each line under "May need you" says '
                 'where it holds; if nothing needs you, end with one line.' if woke
                 else 'If nothing needs you, end with one line.')
    return '\n'.join(lines)


def every(row):
    """The note that tells the lead every message, for one event."""
    task, turn = row['task'], row['turn']
    handle = f'turn:{task}/{turn}'
    lines = ['Task updates: what happened in your tasks since you last heard; the wait tool reads each turn by its handle.']
    quoted = False
    if row['kind'] == 'Said':
        if row.get('from') == PERSON:
            lines.append(f'- {task}: {handle}, the person wrote: "{cut(words(row.get("words") or ""), QUOTE_CHARS)}"')
            quoted = True
        else:
            lines.append(f'- {task}: {handle}, {row.get("from") or "someone"} wrote in it')
    else:
        line, theirs = ended_line(row)
        if theirs:
            lines.append('The person asked for these in the task themselves, so they are theirs:')
        lines.append(line)
    if quoted:
        lines.append("A quoted message is the person's own words.")
    lines.append('Read the turns you need with one wait call (timeout_ms 0), and act where something changes the '
                 'work. Tell the person, briefly, what they would want to hear: the outcome of work you started '
                 'for them, once, when it ends, not again when they write in that task; what only they can '
                 'decide; and where their latest words contradict what they asked before. If nothing, end your '
                 'turn with an empty reply.')
    return '\n'.join(lines)


REHEARSAL = """[Dry run] This is a rehearsal of how you would handle the message below; it is not the real one. \
Every tool but wait is off for it, and nothing you write reaches anyone. Read what you need with wait as you \
normally would. Then, for each thing you would do with another tool (message or steer a task, start one, save \
memory, anything else), write one line that starts with `WOULD: `. After those lines, write exactly what you \
would reply to the person, or nothing if you would reply nothing.

"""


def turn_items(daemon, bot, turn):
    """The fork's calls and its reply in its turn."""
    # Newest first, back to the rehearsal's first node: the history it
    # inherits from the lead comes before it.
    nodes, start = [], None
    while True:
        page = daemon.request('history_nodes', bot=bot, limit=400, **({'from': start} if start else {}))
        nodes += [n['node'] for n in page['nodes'] if n.get('turn') == turn]
        older = page['nodes'] and page['nodes'][-1].get('turn') == turn
        start = page.get('next_from')
        if not older or start is None:
            break
    nodes.sort()
    if not nodes:
        return []
    # At most 400 a read, and a read cut short by size goes on from the
    # first node it did not return.
    items = []
    while nodes:
        read = daemon.request('history_items', bot=bot, nodes=nodes[:400])
        got = read['items']
        if not got:
            break
        items += [entry['item'] for entry in got]
        seen = {entry['node'] for entry in got}
        nodes = [node for node in nodes if node not in seen]
    return items


def calls(items):
    found = []
    for item in items:
        if item.get('type') == 'function_call':
            found.append({'name': item.get('name'), 'arguments': item.get('arguments')})
        for block in item.get('content') or []:
            if isinstance(block, dict) and block.get('type') == 'tool_use':
                found.append({'name': block.get('name'), 'arguments': json.dumps(block.get('input'))})
    return found


def run_one(socket_path, row, arm, note, n):
    daemon = Daemon(socket_path)
    bot = f'lookout-eval.{arm}.{n}.{int(time.time() * 1000) % 100000}'
    out = {'task': row['task'], 'turn': row['turn'], 'kind': row['kind'], 'lead': row['lead'], 'arm': arm,
           'n': row['n'], 'note': note}
    try:
        forked = False
        for node in row.get('nodes') or []:
            try:
                daemon.request('fork', source=row['lead'], bot=bot, checkpoint=node, allow=['wait'],
                               expire_ms=900_000)
                forked = True
                break
            except Failed as e:
                if e.code in ('fork_point_has_open_tool_calls', 'fork_point_splits_reasoning'):
                    continue
                raise
        if not forked:
            out['failed'] = 'no fork point'
            return out
        submitted = daemon.request('submit', bot=bot, request_id=bot, prompt=REHEARSAL + note, delivery='queue')
        handle = submitted['handle']
        waited = daemon.request('wait', handles=[handle], timeout_ms=TURN_SECONDS * 1000)
        outcome = waited['results'][handle]
        if outcome.get('pending'):
            daemon.request('interrupt', bot=bot, turn=submitted['turn'])
            out['failed'] = 'timed out'
            return out
        out['status'] = outcome.get('status')
        if out['status'] != 'completed':
            out['failed'] = f"turn {out['status']}"
            return out
        text = outcome.get('text') or ''
        out['would'] = [l[len('WOULD:'):].strip() for l in text.splitlines() if l.strip().startswith('WOULD:')]
        out['reply'] = '\n'.join(l for l in text.splitlines() if not l.strip().startswith('WOULD:')).strip()
        out['calls'] = calls(turn_items(daemon, bot, submitted['turn']))
        row = daemon.request('turns', bot=bot, after=submitted['turn'] - 1, limit=1)['turns'][0]
        out['usage'] = {k: row.get(k) for k in ('input_tokens', 'cached_input_tokens', 'output_tokens')}
    except Failed as e:
        out['failed'] = str(e)
    finally:
        try:
            daemon.request('delete', bot=bot)
        except Failed:
            pass
        daemon.close()
    return out


def load(path):
    return [json.loads(l) for l in open(path) if l.strip()]


def run(args):
    rows = {}
    for row in load(args.replay):
        rows.setdefault(key(row), row)
    bands = {}
    if args.bands:
        for row in load(args.bands):
            if 'band' in row and (not args.band_judge or row.get('judge') == args.band_judge):
                bands.setdefault(key(row), row)
    if args.labels:
        wanted = set(labelled(args.labels, args.old))
        rows = {k: r for k, r in rows.items() if event(k) in wanted}
    if 'today' in args.arms.split(','):
        missing = sum(k not in bands and (None, *event(k)) not in bands for k in rows)
        print(f'{missing} of {len(rows)} events have no band: today sends them as no judge picked them',
              file=sys.stderr)
    arms = args.arms.split(',')
    done = set()
    try:
        for row in load(args.out):
            if 'failed' not in row:
                done.add((*key(row), row['arm']))
    except FileNotFoundError:
        pass
    work = []
    for n, (k, row) in enumerate(sorted(rows.items(), key=lambda kv: kv[1]['n'])):
        for arm in arms:
            if (*k, arm) in done:
                continue
            # A replay from before rows named their lead matches any lead.
            band = bands.get(k, bands.get((None, *event(k))))
            note = today(row, band) if arm == 'today' else every(row)
            work.append((row, arm, note, n))
    unsent = [w for w in work if w[2] is None]
    work = [w for w in work if w[2] is not None]
    print(f'{len(work)} turns to run, {len(unsent)} unsent', file=sys.stderr)
    lock = threading.Lock()
    with open(args.out, 'a') as sink, ThreadPoolExecutor(args.jobs) as pool:
        for row, arm, _, _ in unsent:
            sink.write(json.dumps({'task': row['task'], 'turn': row['turn'], 'kind': row['kind'],
                                   'lead': row['lead'], 'arm': arm, 'n': row['n'], 'note': None,
                                   'unsent': True, 'would': [], 'reply': ''}) + '\n')
        for out in pool.map(lambda w: run_one(args.socket, *w), work):
            with lock:
                sink.write(json.dumps(out) + '\n')
                sink.flush()
            print(out['arm'], out['task'], out['turn'], out['kind'], out.get('failed', 'ok'), file=sys.stderr)


GRADER = """You grade how a lead agent handled one note from its lookout. The lead coordinates task agents \
for a person. The note told it about one event in a task. It was a dry run: the lead wrote each tool call it \
would make as a WOULD line, then what it would reply to the person.

The user message is a JSON object. `event` is what happened. `label` is a hand label: `tell` says whether the \
lead should act on or know about this event at all, and `why` says what the right handling is. `note` is what \
the lead was told. `waited` lists the turns it read with its wait tool. `would` and `reply` are what it would \
do and say.

Answer with one JSON object and nothing else:
{"handled": "yes" | "partly" | "no" | "n/a", "needless": true | false, "reply": "silent" | "filler" | "news", \
"why": "one sentence"}
- handled: when `tell` is true, whether the lead's actions and reply do what `why` calls for, or something \
as good. "no" when it let it go or did something else. When `tell` is false, "n/a".
- needless: whether the lead did or would say anything nothing called for: messaging, steering or starting \
a task, or telling the person something they did not need. A wait call is never needless.
- reply: "silent" for an empty reply, "filler" for one that tells the person nothing they need (that \
nothing needs them, or that it noted something), "news" for one that tells them something."""


def grade_one(socket_path, model, turn, label, event, n):
    daemon = Daemon(socket_path)
    bot = f'lookout-grade.{n}.{int(time.time() * 1000) % 100000}'
    out = dict(turn)
    out['label'] = {k: label.get(k) for k in ('tell', 'cat', 'why')}
    try:
        daemon.request('create', bot=bot, workspace='/', model=model, instructions=GRADER, tools=[])
        waited = [h for c in turn.get('calls', []) if c['name'] == 'wait'
                  for h in (json.loads(c['arguments'] or '{}').get('handles') or [])]
        prompt = json.dumps({'event': event, 'label': out['label'], 'note': turn['note'], 'waited': waited,
                             'would': turn.get('would'), 'reply': turn.get('reply')})
        submitted = daemon.request('submit', bot=bot, request_id=bot, prompt=prompt, delivery='queue')
        handle = submitted['handle']
        outcome = daemon.request('wait', handles=[handle], timeout_ms=180_000)['results'][handle]
        text = (outcome.get('text') or '').strip()
        start, end = text.find('{'), text.rfind('}')
        out['grade'] = json.loads(text[start:end + 1])
    except (Failed, ValueError) as e:
        out['grade_failed'] = str(e)
    finally:
        try:
            daemon.request('delete', bot=bot)
        except Failed:
            pass
        daemon.close()
    return out


def labelled(path, old_path):
    """Labels by task, turn and kind; those keyed by `n` through the replay
    they were made for."""
    old = {r['n']: event(key(r)) for r in load(old_path)} if old_path else {}
    labels = {}
    for label in load(path):
        k = (label['task'], label['turn'], label['kind']) if 'task' in label else old.get(label.get('n'))
        if k:
            labels[k] = label
    return labels


def grade(args):
    labels = labelled(args.labels, args.old)
    events = {}
    for row in load(args.replay) if args.replay else []:
        events.setdefault(key(row), {'task': row['task'], 'kind': row['kind'], 'from': row.get('from'),
                                     'status': row.get('status'), 'text': row.get('text')})
    work = []
    for n, turn in enumerate(load(args.turns)):
        k = key(turn)
        if 'failed' in turn or event(k) not in labels:
            continue
        work.append((turn, labels[event(k)], events.get(k, {'task': turn['task'], 'kind': turn['kind']}), n))
    print(f'{len(work)} turns to grade', file=sys.stderr)
    with open(args.out, 'w') as sink, ThreadPoolExecutor(args.jobs) as pool:
        # A note never sent left the lead doing nothing.
        for turn, label, _, _ in [w for w in work if w[0].get('unsent')]:
            out = dict(turn, label={k: label.get(k) for k in ('tell', 'cat', 'why')})
            out['grade'] = {'handled': 'no' if label['tell'] else 'n/a', 'needless': False, 'reply': 'silent',
                            'why': 'the note was not sent'}
            sink.write(json.dumps(out) + '\n')
        work = [w for w in work if not w[0].get('unsent')]
        for out in pool.map(lambda w: grade_one(args.socket, args.grader, *w), work):
            sink.write(json.dumps(out) + '\n')
            sink.flush()


def score(args):
    rows = [r for r in load(args.grades) if 'grade' in r]
    for arm in sorted({r['arm'] for r in rows}):
        mine = [r for r in rows if r['arm'] == arm]
        pos = [r for r in mine if r['label']['tell']]
        neg = [r for r in mine if not r['label']['tell']]
        handled = sum(r['grade'].get('handled') == 'yes' for r in pos)
        partly = sum(r['grade'].get('handled') == 'partly' for r in pos)
        needless = sum(bool(r['grade'].get('needless')) for r in mine)
        replies = {k: sum(r['grade'].get('reply') == k for r in neg) for k in ('silent', 'filler', 'news')}
        tokens = sorted((r.get('usage') or {}).get('input_tokens', 0) for r in mine)
        print(f'{arm}: {len(mine)} turns; positives {len(pos)}: handled {handled}, partly {partly}, '
              f'missed {len(pos) - handled - partly}; needless {needless} '
              f'({sum(bool(r["grade"].get("needless")) for r in neg)} on negatives); '
              f'replies on negatives {replies}; median input tokens {tokens[len(tokens) // 2] if tokens else 0}')
        cats = sorted({r['label'].get('cat') or '' for r in pos})
        for cat in cats:
            these = [r for r in pos if (r['label'].get('cat') or '') == cat]
            print(f'  {cat or "-"}: {sum(r["grade"].get("handled") == "yes" for r in these)}/{len(these)} handled')


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest='command', required=True)
    r = sub.add_parser('run')
    r.add_argument('replay')
    r.add_argument('--socket', required=True)
    r.add_argument('--out', required=True)
    r.add_argument('--bands')
    r.add_argument('--band-judge')
    r.add_argument('--labels')
    r.add_argument('--old')
    r.add_argument('--arms', default='today,every')
    r.add_argument('--jobs', type=int, default=4)
    g = sub.add_parser('grade')
    g.add_argument('turns')
    g.add_argument('labels')
    g.add_argument('--socket', required=True)
    g.add_argument('--grader', required=True)
    g.add_argument('--out', required=True)
    g.add_argument('--old')
    g.add_argument('--replay')
    g.add_argument('--jobs', type=int, default=4)
    s = sub.add_parser('score')
    s.add_argument('grades')
    args = parser.parse_args()
    {'run': run, 'grade': grade, 'score': score}[args.command](args)


if __name__ == '__main__':
    main()
