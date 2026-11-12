"""Compaction instructions: a before/after eval of the summaries they produce.

For each chosen long history in a COPY of the person's store, write the span a
summary would cover as text (the previous summary first, as the summary request
sends it), have a fresh ChatGPT-plan bot summarize it under the OLD and the NEW
instructions (separate bots, no tools, no compaction), then have a judge bot score
both summaries blind against the transcript. Nothing here names a person, a
company or a machine; transcripts and summaries stay under --out (local only).
Only aggregate numbers go in the report.

    python3 summary_eval.py --store /tmp/eval-state.sqlite --bots A,B,C \
        --agent PATH/TO/agent --trials 2 --out /tmp/summary-eval

Runs on a scratch daemon of its own (`--store` under --out) with only the chatgpt
provider, so it never touches the person's daemon or spends an API key; the ChatGPT
login is read in place. A rerun resumes from results.jsonl.
"""
import argparse
import json
import random
import sqlite3
import statistics
import subprocess
from pathlib import Path

MODEL = 'chatgpt/gpt-6.1-sol'
SPAN_BYTES = 200 * 1024
PROMPT_BYTES = 250 * 1024  # under `agent run`'s prompt limit
ITEM_BYTES = 3000

OLD = ("You are summarizing the earlier part of an agent's conversation so the agent can continue "
       "with the summary in place of those turns. Any earlier summary is given first; merge it with the new turns, do not restart. "
       "Write, in order: the goal; every rule, constraint, or preference the user stated, verbatim where wording matters; "
       "what is done, in progress, and blocked; key decisions and why; files read or changed; open questions; next steps. "
       "Keep exact names, paths, commands, values, and error text. Omit chatter, repeated tool output, and anything superseded. "
       "Reply with the summary only.")
NEW = ("You are summarizing the earlier part of an agent's conversation so the agent can continue "
       "with the summary in place of those turns. Any earlier summary is given first; merge it with the new turns, do not restart. "
       "Write, in order: the goal; every rule, constraint, or preference the user stated, verbatim where wording matters; "
       "what is done, in progress, and blocked; key decisions and why; files read or changed; open questions; next steps. "
       "Keep exact names, paths, commands, values, and error text. "
       "Mark each decision as the user's (quote their words and give when they said them), an agent's plan the user has not agreed to, or your inference. "
       "Give each statement of current state the time it was last checked. "
       "For each open item, keep the id or source that would close it. "
       "End with a list of what is not verified, for the agent to check before relying on it. "
       "Omit chatter, repeated tool output, and anything superseded. "
       "Reply with the summary only.")

JUDGE = """You grade two summaries (A and B) of the same agent conversation, written so the
agent can continue from the summary alone. The conversation is below, then A, then B.
Grade each summary on its own against the conversation. Reply with JSON only:
{"A": GRADES, "B": GRADES, "better": "A"|"B"|"tie", "why": "one sentence"}
where GRADES is
{"decisions": number of decisions the summary states,
 "attributed": how many of those say who made them (the user, an agent's unagreed plan, or inference),
 "misattributed": how many present an agent's plan or an inference as the user's decision, or attribute wrongly,
 "user_quotes_with_time": how many user decisions quote the user and give when they said it,
 "state_lines": number of statements of current state,
 "state_with_time": how many of those give when they were last checked,
 "open_items": number of open items,
 "open_with_closer": how many carry an id or source that would close them,
 "not_verified_list": true if it ends with a list of what is not verified,
 "not_verified_items": number of items in that list,
 "unsupported": number of claims the conversation does not support,
 "missing": number of user rules, constraints or identifiers in the conversation that a continuing agent needs and the summary leaves out,
 "usable": 1-5, how well an agent could continue from this summary alone}
"""


def render(item):
    """One history item as short text: who, and what it said or did."""
    try:
        v = json.loads(item)
    except (TypeError, ValueError):
        return ''
    out = []
    role = v.get('role') or v.get('type') or '?'
    content = v.get('content')
    if isinstance(content, str):
        out.append(content)
    elif isinstance(content, list):
        for b in content:
            if not isinstance(b, dict):
                continue
            t = b.get('type', '')
            if t in ('thinking', 'redacted_thinking', 'reasoning'):
                continue
            if 'text' in b and isinstance(b['text'], str):
                out.append(b['text'])
            elif t == 'tool_use':
                out.append(f"[call {b.get('name')}] {json.dumps(b.get('input'))}")
            elif t == 'tool_result':
                c = b.get('content')
                if isinstance(c, list):
                    c = ' '.join(x.get('text', '') for x in c if isinstance(x, dict))
                out.append(f"[result] {c}")
    if v.get('type') == 'function_call':
        out.append(f"[call {v.get('name')}] {v.get('arguments')}")
    if v.get('type') == 'function_call_output':
        out.append(f"[result] {v.get('output')}")
    text = '\n'.join(x for x in out if x)
    if len(text) > ITEM_BYTES:
        text = text[:ITEM_BYTES] + ' …'
    return f"{role}: {text}" if text else ''


def tail(text, limit):
    """The newest `limit` bytes of text."""
    data = text.encode()
    return text if len(data) <= limit else data[-limit:].decode('utf-8', 'ignore')


def span(store, bot):
    """The previous summary and the history since the bot's last cut, as text,
    SPAN_BYTES in all: the summary whole, then the newest of the history."""
    db = sqlite3.connect(f'file:{store}?mode=ro', uri=True)
    head, compaction = db.execute('SELECT head,compaction FROM bots WHERE name=?', (bot,)).fetchone()
    cut, summary = None, ''
    if compaction:
        row = db.execute('SELECT cut,summary FROM compactions WHERE node=?', (compaction,)).fetchone()
        if row:
            cut, summary = row
    lines, node = [], head
    while node is not None:
        parent, item = db.execute('SELECT parent,item FROM nodes WHERE id=?', (node,)).fetchone()
        if isinstance(item, bytes):
            item = item.decode('utf-8', 'replace')
        lines.append(render(item))
        if node == cut:
            break
        node = parent
    text = '\n\n'.join(x for x in reversed(lines) if x)
    if not summary:
        return tail(text, SPAN_BYTES)
    head = f"[earlier summary]\n{summary}\n\n[conversation since]\n"
    return head + tail(text, max(SPAN_BYTES - len(head.encode()), 0))


def ask(agent, store, bot, instructions, prompt):
    """A fresh bot's reply to one prompt, on the scratch daemon."""
    out = subprocess.run([agent, 'run', '--store', store, '--provider', 'chatgpt', '--new', '--bot', bot,
                          '--model', MODEL, '--tools', '', '--no-compaction', '--instructions', instructions,
                          '--', '-'],
                         input=prompt, capture_output=True, text=True, timeout=1800, check=False)
    text = ''.join(json.loads(l).get('text', '') for l in out.stdout.splitlines()
                   if l.startswith('{') and json.loads(l).get('event') == 'text_delta')
    if out.returncode != 0 or not text.strip():
        raise RuntimeError(f'{bot}: exit {out.returncode}: {out.stderr[-400:]}')
    return text


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--store', required=True)
    ap.add_argument('--bots', required=True)
    ap.add_argument('--agent', required=True)
    ap.add_argument('--trials', type=int, default=2)
    ap.add_argument('--out', required=True)
    a = ap.parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    store = str(out / 'scratch.sqlite')
    run = random.randrange(1 << 32)  # scratch bot names stay unique across reruns
    done = out / 'results.jsonl'
    results = [json.loads(l) for l in done.read_text().splitlines()] if done.exists() else []
    for i, bot in enumerate(a.bots.split(',')):
        transcript = span(a.store, bot)
        (out / f'h{i}.txt').write_text(transcript)
        for trial in range(a.trials):
            if any(r['history'] == i and r['trial'] == trial for r in results):
                continue
            summaries = {}
            for arm, instructions in (('old', OLD), ('new', NEW)):
                summaries[arm] = ask(a.agent, store, f'sum-{arm}-{i}-{trial}-{run}', instructions, transcript)
                (out / f'h{i}-t{trial}-{arm}.md').write_text(summaries[arm])
            order = ['old', 'new']
            random.shuffle(order)
            rest = f"\n\n[summary A]\n{summaries[order[0]]}\n\n[summary B]\n{summaries[order[1]]}"
            room = PROMPT_BYTES - len(rest.encode()) - len('[conversation]\n')
            # Over the limit, the judge's copy drops the oldest conversation,
            # never the earlier summary.
            head, sep, since = transcript.partition('[conversation since]\n')
            head, since = (head + sep, since) if sep else ('', transcript)
            since = tail(since, max(room - len(head.encode()), 0))
            prompt = f"[conversation]\n{head}{since}{rest}"
            graded = None
            for attempt in range(2):
                raw = ask(a.agent, store, f'judge-{i}-{trial}-{run}-{attempt}', JUDGE, prompt)
                (out / f'h{i}-t{trial}-judge{attempt}.txt').write_text(raw)
                try:
                    graded = json.loads(raw[raw.index('{'):raw.rindex('}') + 1])
                    break
                except ValueError:
                    continue
            if graded is None:
                raise RuntimeError(f'history {i} trial {trial}: the judge gave no JSON twice')
            row = {'history': i, 'trial': trial,
                   'better': {'A': order[0], 'B': order[1]}.get(graded.get('better'), 'tie')}
            for label, arm in zip('AB', order):
                row[arm] = {**graded[label], 'bytes': len(summaries[arm].encode())}
            results.append(row)
            with done.open('a') as f:
                f.write(json.dumps(row) + '\n')
            print(json.dumps(row), flush=True)
    report(results, out)


def report(results, out):
    def mean(arm, f):
        vals = [r[arm][f] for r in results if isinstance(r[arm].get(f), (int, float))]
        return statistics.mean(vals) if vals else 0

    def share(arm, part, whole):
        p, w = sum(r[arm].get(part, 0) for r in results), sum(r[arm].get(whole, 0) for r in results)
        return f'{p}/{w}'
    lines = ['| measure | old | new |', '|---|---|---|']
    for label, part, whole in [('decisions attributed', 'attributed', 'decisions'),
                               ('state lines with a time', 'state_with_time', 'state_lines'),
                               ('open items with a closer', 'open_with_closer', 'open_items')]:
        lines.append(f'| {label} | {share("old", part, whole)} | {share("new", part, whole)} |')
    for f in ('misattributed', 'user_quotes_with_time', 'not_verified_items', 'unsupported', 'missing',
              'usable', 'bytes'):
        lines.append(f'| {f} (mean) | {mean("old", f):.1f} | {mean("new", f):.1f} |')
    nv = lambda arm: sum(bool(r[arm].get('not_verified_list')) for r in results)
    lines.append(f'| ends with a not-verified list | {nv("old")}/{len(results)} | {nv("new")}/{len(results)} |')
    wins = {k: sum(r['better'] == k for r in results) for k in ('old', 'new', 'tie')}
    lines.append(f'| judge prefers | {wins["old"]} | {wins["new"]} (ties {wins["tie"]}) |')
    (out / 'report.md').write_text('\n'.join(lines) + '\n')
    print('\n'.join(lines))


if __name__ == '__main__':
    main()
