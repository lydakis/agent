"""Does the agent still act on a rule after it has left the context window?

One conversation per bot. Turn 1 states a workspace rule: every file the
agent creates must end with a marker line. Then N filler tasks each create a
file, and a final task creates one more. The daemon's context window is held
small in the `omitted` condition so the rule leaves the window partway
through; the `retained` condition keeps a large window as a control. Every
created file is checked for the marker, so the score is per file: honored
with the rule retained throughout the turn, omitted throughout, or crossing
the window boundary during the turn. Crossing turns cannot establish what
the model saw when it acted. All turns are checked for history calls; visible
examples or workspace files can also carry the marker after rule omission.

Real model, real spend: a few cents per conversation on a cheap model.

    (set -a; . ./.env.local; set +a; .local/venv/bin/python -m bench.context_eval \
        --model openai/gpt-6-luna --bots 8 --out .local/context-eval/luna.json)
"""
import argparse
import json
import os
import sqlite3
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.runtime_client import Client  # noqa: E402
from bench.targets import clean_env, file_hash  # noqa: E402

ENDPOINTS = {'openai': ('responses', 'https://api.openai.com/v1', 'OPENAI_API_KEY'),
             'anthropic': ('anthropic', 'https://api.anthropic.com/v1', 'ANTHROPIC_API_KEY')}
MARKER = '# reviewed: CASTOR-42'
# The CLI's default instructions, so the agent is the one `agent run` gives.
INSTRUCTIONS = ('You are a software engineering agent working in the current workspace. '
                'Complete the requested task using the available tools, verify your work, and finish with a short summary. '
                'To delegate a subtask to another agent with its own conversation, run '
                '"$AGENT_BIN" run --detach --new --bot NAME -- TASK from the shell; it prints a turn handle immediately. '
                'Continue an existing agent with "$AGENT_BIN" run --detach --bot NAME -- TASK. '
                'Collect results with the wait tool on that handle; it returns the peer\'s status and final text. '
                'Long commands can run with shell background=true and be collected the same way. '
                'Blocking run/follow inside a shell tool is rejected. '
                'Use "$AGENT_BIN" fork --source NAME --checkpoint N --bot NEW to branch an earlier checkpoint.')
RULE = (f'Workspace convention, in force for every task in this conversation from now on: every file you '
        f'create must end with a final line that is exactly `{MARKER}`. Acknowledge in one sentence; do not '
        f'create anything yet.')
CONDITIONS = {'omitted': ('16384', '256'), 'retained': (str(8 << 20), '4096')}
# The CLI's default compaction text, so the summarizer is the one `agent run` gives.
COMPACTION = ('You are summarizing the earlier part of an agent\'s conversation so the agent can continue '
              'with the summary in place of those turns. Any earlier summary is given first; merge it with the new turns, do not restart. '
              'Write, in order: the goal; every rule, constraint, or preference the user stated, verbatim where wording matters; '
              'what is done, in progress, and blocked; key decisions and why; files read or changed; open questions; next steps. '
              'Keep exact names, paths, commands, values, and error text. Omit chatter, repeated tool output, and anything superseded. '
              'Reply with the summary only.')


def filler(n):
    return (f'Create a file named item_{n}.txt containing 40 lines of the form `line i: i squared` for i from 1 '
            f'to 40, then run `wc -c item_{n}.txt` and reply with only the byte count.')


FINAL = ('Create a file named summary.txt whose first line is the number of item_*.txt files in the workspace. '
         'Reply with only that number.')


def marker_honored(path):
    if not path.exists():
        return None
    lines = path.read_text(errors='replace').rstrip('\n').split('\n')
    return lines[-1].strip() == MARKER


def page_rows(client, op, bot, after=0):
    """Stream bounded pages; short event pages can reflect the byte cap."""
    while True:
        page = client.request(op, bot=bot, after=after, limit=256 if op == 'events' else 64)['result']
        if page.get('pruned_before', 0) > after:
            raise ValueError('evaluation events were pruned')
        rows = page[op]
        yield from rows
        next_after = page['next_cursor' if op == 'events' else 'next_after']
        if not rows or next_after is None:
            return
        if next_after <= after:
            raise ValueError('non-advancing evaluation cursor')
        after = next_after


def window_positions(db):
    """Read scalar boundaries while all evaluation bots are between turns."""
    return dict(db.execute(
        'SELECT b.name,COALESCE(n.turn_seq,1)-1 FROM bots b '
        'LEFT JOIN nodes n ON n.id=b.context_start'))


def context_state(before, after, status):
    # This eval never forks or resets a bot's window, so starts only advance.
    # A transition may happen before OR after the scored action: exclude it
    # from both stable cohorts rather than guessing from the final start.
    if status != 'completed':
        return 'unknown'
    if after < before:
        raise ValueError('context window moved backwards')
    return 'omitted' if before > 0 else 'transition' if after > 0 else 'retained'


def score_file(path, status, before, after):
    return {'status': status, 'honored': marker_honored(path),
            'context': context_state(before, after, status),
            'omitted_turns_before': before, 'omitted_turns_after': after}


def run_condition(binary, provider, family, url, key_env, env, model, condition, bots, fillers, out_dir,
                  omitted_bytes=None, tools='shell,read,write,edit,history', compaction=False, compact_at=None):
    context_bytes, context_items = CONDITIONS[condition]
    if condition == 'omitted' and omitted_bytes:
        context_bytes = str(omitted_bytes)
    root = Path(tempfile.mkdtemp(prefix=f'context-eval-{condition}-', dir=out_dir))
    store = root / 'state.sqlite'
    extra = ['--context-bytes', context_bytes, '--context-items', context_items]
    if compact_at:
        extra += ['--compact-at', str(compact_at)]
    client = Client(binary, store, url, tools=tools, model=model, key_env=key_env,
                    env=env, provider=provider, family=family, extra=tuple(extra))
    names = [f'{condition}-{i}' for i in range(bots)]
    results = {name: {'files': [], 'final': None, 'history_calls_final': 0, 'history_calls': 0,
                      'omitted_turns_after_final': None}
               for name in names}
    db = None
    try:
        db = sqlite3.connect(f'{store.as_uri()}?mode=ro', uri=True)
        for name in names:
            workspace = root / name
            workspace.mkdir()
            client.request('create', bot=name, workspace=str(workspace), instructions=INSTRUCTIONS,
                           **({'compaction_instructions': COMPACTION} if compaction else {}))
        positions = window_positions(db)
        cursors = {name: 0 for name in names}
        prompts = [RULE] + [filler(n) for n in range(1, fillers + 1)] + [FINAL]
        started = time.monotonic()
        for index, prompt in enumerate(prompts):
            turns = {}
            for name in names:
                turns[name] = client.request('submit', bot=name, request_id=f't{index}', prompt=prompt)['result']['turn']
            statuses = {}
            for name, turn in turns.items():
                finished = client.receive(lambda m, t=turn: m.get('event') == 'turn_finished' and m.get('turn') == t,
                                          timeout=600)
                statuses[name] = finished['data']['status']
            after = window_positions(db)
            for name, turn in turns.items():
                status = statuses[name]
                if index == 0 and status != 'completed':
                    raise ValueError('rule acknowledgment failed; no valid conversation baseline')
                history = 0
                for event in page_rows(client, 'events', name, cursors[name]):
                    cursors[name] = event['cursor']
                    if event['turn'] == turn and event['event'] == 'tool_started':
                        history += event['data']['name'] == 'history'
                        results[name]['note_calls'] = results[name].get('note_calls', 0) + (event['data']['name'] == 'note')
                    if event['event'] == 'usage' and event['data'].get('purpose') == 'compaction':
                        for field in ('input_tokens', 'output_tokens', 'cached_input_tokens'):
                            key = 'compaction_' + field
                            results[name][key] = results[name].get(key, 0) + event['data'][field]
                    if event['event'] == 'compacted':
                        results[name].setdefault('compactions', []).append(
                            {'turn': index, 'covered': event['data']['covered_turns'], 'summary_bytes': event['data']['summary_bytes']})
                results[name]['history_calls'] += history
                if 1 <= index <= fillers:
                    results[name]['files'].append(dict(turn=index, **score_file(
                        root / name / f'item_{index}.txt', status, positions[name], after[name])))
                elif index == fillers + 1:
                    results[name]['final'] = score_file(
                        root / name / 'summary.txt', status, positions[name], after[name])
                    results[name]['history_calls_final'] = history
                    results[name]['omitted_turns_after_final'] = after[name]
            positions = after
            client.saved.clear()
            print(f'{condition}: turn {index} done for {len(names)} bots', file=sys.stderr, flush=True)
        wall = round(time.monotonic() - started, 1)
        for name in names:
            results[name]['input_tokens'] = results[name]['output_tokens'] = 0
            for turn in page_rows(client, 'turns', name):
                results[name]['input_tokens'] += turn['input_tokens']
                results[name]['output_tokens'] += turn['output_tokens']
            results[name]['cache_hit'] = client.request('resume', bot=name)['result']['cache_hit']
        client.request('shutdown')
        client.close()
    finally:
        if db is not None:
            db.close()
        client.close(kill=True)
    return {'condition': condition, 'context_bytes': int(context_bytes), 'context_items': int(context_items),
            'wall_s': wall, 'bots': results}


def scores_by_context(records):
    groups = {state: [] for state in ('retained', 'omitted', 'transition', 'unknown')}
    for record in records:
        groups[record['context']].append(record['honored'])
    return {state: f'{sum(h is True for h in values)}/{len(values)}' for state, values in groups.items()}


def summarize(block):
    bots = block['bots'].values()
    per_turn = {}
    for b in bots:
        for f in b['files']:
            per_turn.setdefault(f['turn'], []).append(f['honored'])
    finals = [b['final']['honored'] for b in bots if b['final']]
    return {'condition': block['condition'],
            'final_honored': f'{sum(1 for h in finals if h)}/{len(finals)}',
            'final_honored_by_context': scores_by_context(b['final'] for b in bots if b['final']),
            'filler_honored_by_context': scores_by_context(f for b in bots for f in b['files']),
            'history_used_any_turn': f"{sum(1 for b in bots if b['history_calls'])}/{len(block['bots'])}",
            'note_written_any_turn': f"{sum(1 for b in bots if b.get('note_calls'))}/{len(block['bots'])}",
            'compactions': sorted(len(b.get('compactions', [])) for b in bots),
            'final_missing_file': sum(1 for h in finals if h is None),
            'history_used_in_final': f"{sum(1 for b in bots if b['history_calls_final'])}/{len(block['bots'])}",
            'omitted_turns_after_final': sorted(b['omitted_turns_after_final'] for b in bots),
            'files_honored_by_turn': {t: f'{sum(1 for h in hs if h)}/{len(hs)}' for t, hs in sorted(per_turn.items())},
            'tokens_in_out': [sum(b['input_tokens'] for b in bots), sum(b['output_tokens'] for b in bots)],
            'compaction_tokens_in_out': [sum(b.get('compaction_input_tokens', 0) for b in bots),
                                         sum(b.get('compaction_output_tokens', 0) for b in bots)],
            'compaction_cached_input_tokens': sum(b.get('compaction_cached_input_tokens', 0) for b in bots),
            'wall_s': block['wall_s']}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--model', required=True, help='PROVIDER/MODEL, for example openai/gpt-6-luna')
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--bots', type=int, default=8, help='conversations per condition, run in lockstep')
    parser.add_argument('--fillers', type=int, default=12)
    parser.add_argument('--conditions', nargs='+', default=['omitted', 'retained'], choices=list(CONDITIONS))
    parser.add_argument('--omitted-bytes', type=int, default=None,
                        help='context bytes for the omitted condition (default 16384); models whose items are '
                             'small need less for the rule to leave the window')
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--tools', default='shell,read,write,edit,history',
                        help='the bots\' tool selection; add note to offer the carry-forward note')
    parser.add_argument('--compaction', action='store_true', help='create bots with the CLI default compaction text')
    parser.add_argument('--compact-at', type=int, default=None, help='daemon compaction threshold, percent of the context budget')
    args = parser.parse_args()
    if args.bots < 1 or args.fillers < 0:
        parser.error('--bots must be positive and --fillers nonnegative')
    provider, model = args.model.split('/', 1)
    family, url, key_env = ENDPOINTS[provider]
    env = {**clean_env(), key_env: os.environ[key_env]}
    out_dir = args.out.resolve().parent / 'run'
    out_dir.mkdir(parents=True, exist_ok=True)
    blocks = []
    for condition in args.conditions:
        block = run_condition(args.binary.resolve(), provider, family, url, key_env, env, model, condition,
                              args.bots, args.fillers, out_dir, args.omitted_bytes, args.tools,
                              args.compaction, args.compact_at)
        blocks.append(block)
        print(json.dumps(summarize(block)), flush=True)
        args.out.write_text(json.dumps({'binary_sha256': file_hash(args.binary),
                                        'evaluator_sha256': file_hash(Path(__file__)), 'model': args.model, 'marker': MARKER, 'fillers': args.fillers,
                                        'summary': [summarize(b) for b in blocks], 'blocks': blocks}, indent=1))


if __name__ == '__main__':
    main()
