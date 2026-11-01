"""Does a turn the pull cord or a restart parked finish its work when it
continues?

Each bot gets one turn: run `./step 1` to `./step 6` in order, one shell call
each, then reply with the six codes they print. Step 3's first run takes long
enough to be running when the halt comes; every other run takes a few
seconds. Once every bot has
started step 3, the halt cuts it off:

- `pause`: the pull cord (`pause`), then `resume` after a short hold.
- `shutdown`: a stop with no grace, then a fresh daemon on the same store,
  which continues the parked turns.

The cut call is answered `daemon_paused` or `daemon_shutdown`, and the turn
continues. It passes when the bot runs step 3 again, then steps 4 to 6, and its
answer carries all six codes: scored from the step log and the answer, not
from the model's account. A bot that ends its turn on the halt's answer, with
steps left, is counted `stopped`, the Monday rehearsal's failure (11-01).

Real model, real spend. Run it on the ChatGPT plan after `agent login
chatgpt`, once per binary and task prompt to compare them (main's prompt from
`git show main:sonny/agents/task.md`):

    .local/venv/bin/python -m bench.resume_eval --model chatgpt/MODEL \\
        --trials 3 --binary .local/target/release/agent --out .local/resume-eval/new.json

A bot that had left the long step before the halt never met it: its trial is
marked `exercised: false` and left out of the counts.
"""
import argparse
import hashlib
import json
import os
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.context_eval import page_rows  # noqa: E402
from bench.runtime_client import Client, node_item  # noqa: E402
from bench.targets import clean_env, file_hash  # noqa: E402

ENDPOINTS = {'chatgpt': ('responses', 'https://api.openai.com/v1', None),
             'openai': ('responses', 'https://api.openai.com/v1', 'OPENAI_API_KEY')}
STEPS = 6
CUT = 3
# Seconds each quick run of a step takes.
QUICK = 2
PREAMBLE = 'You are an agent working in this folder for a person.'
# Sonny's task prompt, whose pull-cord paragraph the bot is told, as Sonny's
# agents are: what a pause is for, and what to do about a call one cut off.
TASK_PROMPT = Path(__file__).resolve().parents[1] / 'sonny/agents/task.md'
PROMPT = (f'Run ./step 1 through ./step {STEPS} in order, each as its own shell command, waiting for each to '
          f'finish before the next; ./step {CUT} can take up to ten minutes, so give it a ten-minute timeout. '
          f'Then reply with the {STEPS} codes they printed, in order.')
STEP = """#!/bin/sh
n="$1"
# Only the first run of the long step is long: a rerun after the halt is quick.
if [ "$n" = {cut} ] && ! grep -qx "start $n" steps.log 2>/dev/null; then wait={long}; else wait={quick}; fi
echo "start $n" >> steps.log
sleep "$wait"
echo "done $n" >> steps.log
printf 'step %s code: %s\\n' "$n" "$(printf '%s' "{seed}-$n" | shasum | cut -c1-6)"
"""


def code(seed, n):
    return hashlib.sha1(f'{seed}-{n}'.encode()).hexdigest()[:6]


def started(work, n):
    log = work / 'steps.log'
    return log.exists() and f'start {n}' in log.read_text().splitlines()


def instructions(task_prompt):
    """The preamble and the task prompt's paragraph on the pull cord."""
    paragraph = next(p for p in task_prompt.read_text().split('\n\n') if 'pause --reason' in p)
    return PREAMBLE + ' ' + paragraph.strip().replace('"$AGENT_BIN"', 'agent')


def inside_cut(work):
    """Whether the long step is running: started once and not done."""
    lines = (work / 'steps.log').read_text().splitlines()
    return lines.count(f'start {CUT}') == 1 and f'done {CUT}' not in lines


def score(work, seed, answer, status):
    lines = (work / 'steps.log').read_text().splitlines() if (work / 'steps.log').exists() else []
    done = sorted({int(line.split()[1]) for line in lines if line.startswith('done ')})
    codes = [n for n in range(1, STEPS + 1) if code(seed, n) in answer]
    passed = done == list(range(1, STEPS + 1)) and len(codes) == STEPS
    # Ended its turn with work left, after the halt's answer.
    stopped = status == 'completed' and STEPS not in done
    return {'passed': passed, 'stopped': stopped, 'steps_done': done, 'codes_in_answer': codes,
            'step_runs': {n: lines.count(f'start {n}') for n in range(1, STEPS + 1)}}


def run_condition(binary, spec, model, condition, trials, out_dir, env, seed, hold, timeout,
                  task_prompt=TASK_PROMPT):
    provider, family, url, key_env = spec
    root = Path(tempfile.mkdtemp(prefix=f'resume-{condition}-', dir=out_dir))
    store = root / 'state.sqlite'

    def daemon():
        return Client(binary, store, url, tools='shell', model=model, key_env=key_env, env=env,
                      provider=provider, family=family)

    client = daemon()
    names = [f'{condition}-{n}' for n in range(trials)]
    turns, results = {}, {}
    try:
        for n, name in enumerate(names):
            work = root / name
            work.mkdir()
            step = work / 'step'
            # Long enough that every bot is inside it when the halt comes.
            step.write_text(STEP.replace('{cut}', str(CUT)).replace('{long}', '600').replace('{quick}', str(QUICK))
                            .replace('{seed}', str(seed + n)))
            step.chmod(0o755)
            client.request('create', bot=name, workspace=str(work), instructions=instructions(task_prompt), tools=['shell'])
            turns[name] = client.request('submit', bot=name, request_id='steps', prompt=PROMPT)['result']['turn']
        deadline = time.monotonic() + timeout
        while not all(started(root / name, CUT) for name in names):
            if time.monotonic() > deadline:
                raise AssertionError('not every bot reached the long step')
            time.sleep(.5)
        time.sleep(2)
        # A bot that left the long step before the halt never met it: its
        # trial says so and is not scored.
        exercised = {name: inside_cut(root / name) for name in names}
        halted = time.monotonic()
        if condition == 'pause':
            client.request('pause', reason='checking what the agents are doing', by='person')
            time.sleep(hold)
            client.request('resume', by='person')
        else:
            client.request('shutdown')
            client.process.wait(timeout=60)
            client.close(kill=True)
            time.sleep(hold)
            client = daemon()
        ends = {}
        for name in names:
            handle = f'turn:{name}/{turns[name]}'
            while True:
                if time.monotonic() > deadline:
                    raise AssertionError(f'{name} did not finish')
                waited = client.request('wait', handles=[handle], timeout_ms=4_000)['result']
                if handle not in waited['pending']:
                    break
            # Its end as the event log keeps it, with the answer's node.
            ends[name] = next(e['data'] for e in page_rows(client, 'events', name)
                              if e['event'] == 'turn_finished' and e.get('turn') == turns[name])
        for n, name in enumerate(names):
            end = ends[name]
            answer = ''
            if end.get('checkpoint'):
                item = node_item(client, name, end['checkpoint']).get('result', {})
                answer = ''.join(c.get('text', '') for c in item.get('content', []) if isinstance(c, dict))
            results[name] = {'status': end.get('status'), 'error': end.get('error'), 'answer': answer[:2000],
                             'exercised': exercised[name],
                             **score(root / name, seed + n, answer, end.get('status'))}
        client.request('shutdown')
    finally:
        client.close(kill=True)
    return {'condition': condition, 'bots': results, 'halt_to_end_s': round(time.monotonic() - halted, 1)}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--model', required=True, help='PROVIDER/MODEL: chatgpt/MODEL on the plan, or openai/MODEL')
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--trials', type=int, default=3, help='bots per condition, run at once in one daemon')
    parser.add_argument('--conditions', nargs='+', default=['pause', 'shutdown'], choices=['pause', 'shutdown'])
    parser.add_argument('--seed', type=int, default=11)
    parser.add_argument('--hold', type=float, default=10, help='seconds the halt holds')
    parser.add_argument('--timeout', type=float, default=1200, help='seconds for a condition')
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--url', help="another Responses endpoint for the provider, as a scripted model's")
    parser.add_argument('--task-prompt', type=Path, default=TASK_PROMPT,
                        help="a task prompt whose pull-cord paragraph the bots are told, such as main's")
    args = parser.parse_args()
    provider, model = args.model.split('/', 1)
    family, url, key_env = ENDPOINTS[provider]
    url = args.url or url
    env = clean_env()
    if key_env:
        env[key_env] = os.environ[key_env]
    else:
        # The daemon reads the plan login `agent login chatgpt` saved.
        env.update({k: os.environ[k] for k in ('HOME', 'AGENT_CHATGPT_LOGIN') if k in os.environ})
    out_dir = args.out.resolve().parent / 'run'
    out_dir.mkdir(parents=True, exist_ok=True)
    blocks = []
    for condition in args.conditions:
        block = run_condition(args.binary.resolve(), (provider, family, url, key_env), model, condition,
                              args.trials, out_dir, env, args.seed, args.hold, args.timeout, args.task_prompt)
        blocks.append(block)
        bots = [b for b in block['bots'].values() if b['exercised']]
        print(json.dumps({'condition': condition, 'passed': sum(b['passed'] for b in bots),
                          'stopped': sum(b['stopped'] for b in bots), 'trials': len(bots),
                          'not_exercised': len(block['bots']) - len(bots)}), flush=True)
        args.out.write_text(json.dumps({'binary_sha256': file_hash(args.binary), 'model': args.model,
                                        'instructions': instructions(args.task_prompt), 'seed': args.seed,
                                        'conditions': blocks}, indent=1))


if __name__ == '__main__':
    main()
