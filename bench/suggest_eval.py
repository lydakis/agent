"""Does a guess at the person's next message keep its own rules once the
same call may offer an idea or a tip instead of nothing?

Sonny's composer guess (sonny/src/suggest.rs) asks a copy of the agent for
the person's likely next message, or nothing. With `[sonny] suggest_ideas`
on, the same ask goes on to say that, only where it would say nothing, it
may offer a labeled `idea` or `tip`. This runs both asks against the same
agents and compares:

- `obvious`: conversations whose next message is plain (the agent offered
  options, asked to go on, offered an example). A prediction must still
  come back, unlabeled, as often with ideas on as off. An idea or tip here
  is a slot stolen from a prediction.
- `open`: conversations with no obvious next step (thanks, an FYI, a fact
  looked up). With ideas off the copy should mostly say nothing; with them
  on it may offer an idea or a tip, which the run prints to read.

Each fixture is one new agent's first turn, made the way `agent run --new
--agents` makes it, in a HOME and daemon of the run's own, so no real
agent or history is touched. The guesses come from the real code path: an
ignored test in Sonny (`suggest::tests::live`) calls `suggest::guess` on
each agent, both arms, `--rounds` times, one copy at a time.

Real model, real spend or plan quota. Run it on the ChatGPT plan after
`agent login chatgpt`, naming a model `agent models` lists:

    cargo build --release --locked -p agent-runtime
    .local/venv/bin/python -m bench.suggest_eval --model chatgpt/MODEL \\
        --out .local/suggest-eval/MODEL.json

About 7 turns plus 7 x 2 x rounds small guesses, each a cache read of a
short conversation.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.targets import clean_env  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]

# What the person says first, and whether their next message is obvious
# once the agent answers.
FIXTURES = {
    'pick': ('obvious', "Give me three short names for a CLI that syncs dotfiles, numbered. "
                        "Don't do anything else, I'll pick one."),
    'go-on': ('obvious', "List the steps to rotate an SSH key on a server. Give me only the first "
                         "two steps, then ask whether I want the rest."),
    'example': ('obvious', "In two sentences, what is a bloom filter? Then offer to show a short "
                           "Rust example, but don't write it yet."),
    'commit': ('obvious', "Write a one-line commit message for: fixed a race in cache eviction when "
                          "two readers expire the same key. Don't commit anything; ask me if I want "
                          "you to."),
    'thanks': ('open', "That settles the cache question, thanks. Nothing else needed for now, "
                       "just reply briefly."),
    'fyi': ('open', "FYI: the pricing fix merged yesterday and the release went out fine. "
                    "No action needed, just reply briefly."),
    'fact': ('open', "What's the capital of Portugal? One word."),
}


def turn(agent, env, name, workspace, model, timeout, message):
    """A new agent's first turn, to its end: how `run` ended."""
    try:
        run = subprocess.run([*agent, 'run', '--new', '--agents', '--bot', name, '--workspace', str(workspace),
                              '--model', model, '--', message],
                             cwd=workspace, env=env, capture_output=True, text=True, timeout=timeout)
        return {'exit': run.returncode, 'error': run.stderr[-500:] or None}
    except subprocess.TimeoutExpired:
        subprocess.run([*agent, 'interrupt', '--bot', name], env=env, capture_output=True, timeout=30)
        return {'exit': None, 'error': 'timeout'}


def guesses(env, bots, rounds, timeout):
    """What `suggest::tests::live` prints: one guess per agent, arm and round."""
    asked = json.dumps({'bots': bots, 'rounds': rounds})
    run = subprocess.run(['cargo', 'test', '--locked', '-p', 'sonny', '--bin', 'sonny', '--',
                          '--ignored', '--exact', 'suggest::tests::live', '--nocapture'],
                         cwd=ROOT, env={**env, 'SONNY_SUGGEST_EVAL': asked},
                         capture_output=True, text=True, timeout=timeout)
    found = [json.loads(line.split(' ', 1)[1]) for line in run.stdout.splitlines()
             if line.startswith('SUGGEST_EVAL ')]
    if not found:
        sys.exit(f'no guesses:\n{run.stdout[-2000:]}\n{run.stderr[-2000:]}')
    return found


def summarize(rows):
    """Per kind of fixture and arm: how many guesses, labeled ideas and tips,
    and nothing, came back."""
    summary = {}
    for row in rows:
        kind = FIXTURES[row['bot']][0]
        arm = 'ideas_on' if row['ideas'] else 'ideas_off'
        counts = summary.setdefault(kind, {}).setdefault(arm, {'guess': 0, 'idea': 0, 'tip': 0, 'nothing': 0,
                                                               'source': 0})
        if row['text'] is None:
            counts['nothing'] += 1
        elif row['cue'] in ('idea', 'tip'):
            counts[row['cue']] += 1
        elif row['cue'] is None:
            counts['guess'] += 1
        else:
            counts['source'] += 1
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--model', required=True, help='PROVIDER/MODEL: chatgpt/MODEL on the plan')
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--rounds', type=int, default=3, help='guesses per agent and arm')
    parser.add_argument('--provider', help="AGENT_PROVIDER for the daemon (default: the model's provider)")
    parser.add_argument('--binary', type=Path, default=ROOT / '.local/target/release/agent')
    parser.add_argument('--timeout', type=float, default=600, help='seconds a turn may take')
    args = parser.parse_args()
    if not args.binary.exists():
        parser.error(f'{args.binary} is missing: cargo build --release --locked -p agent-runtime')
    env = clean_env()
    env['AGENT_PROVIDER'] = args.provider or args.model.split('/', 1)[0]
    # A chatgpt daemon reads the plan login `agent login chatgpt` saved,
    # which must outlive the run's own HOME.
    if env['AGENT_PROVIDER'] == 'chatgpt':
        env['AGENT_CHATGPT_LOGIN'] = (os.environ.get('AGENT_CHATGPT_LOGIN')
                                      or str(Path.home() / '.agent' / 'chatgpt.json'))
    else:
        env.update({k: v for k, v in os.environ.items() if k.endswith('_API_KEY')})
    # The run's own HOME, outside this checkout, so composing the agents'
    # instructions finds none of this repository's.
    root = Path(tempfile.mkdtemp(prefix='agent-suggest-eval-'))
    if ROOT in root.resolve().parents:
        parser.error(f'{root} is inside {ROOT}: set TMPDIR elsewhere')
    home, workspace = root / 'home', root / 'work'
    (home / '.agent').mkdir(parents=True)
    workspace.mkdir()
    env['HOME'] = str(home)
    # cargo and rustup live in the real HOME.
    for key in ('CARGO_HOME', 'RUSTUP_HOME'):
        env[key] = os.environ.get(key) or str(Path.home() / f'.{key.split("_")[0].lower()}')
    agent = [str(args.binary.resolve())]
    observed = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
    try:
        turns = {name: turn(agent, env, name, workspace, args.model, args.timeout, said)
                 for name, (_, said) in FIXTURES.items()}
        rows = guesses(env, list(FIXTURES), args.rounds, args.timeout * 4)
    finally:
        subprocess.run([*agent, 'shutdown'], env=env, capture_output=True, timeout=60)
    revision = subprocess.run(['git', '-C', str(ROOT), 'describe', '--always', '--dirty', '--abbrev=40'],
                              capture_output=True, text=True).stdout.strip()
    result = {'observed': observed, 'revision': revision, 'model': args.model, 'rounds': args.rounds,
              'summary': summarize(rows), 'turns': turns, 'guesses': rows}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=1))
    print(json.dumps(result['summary'], indent=1))
    for row in rows:
        print(f"{row['bot']:8} {'on ' if row['ideas'] else 'off'} {row['cue'] or '-':6} {row['text']}")
    shutil.rmtree(root, ignore_errors=True)


if __name__ == '__main__':
    main()
