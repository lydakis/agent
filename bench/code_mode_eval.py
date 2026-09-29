"""Does calling MCP tools from code beat calling them one by one?

Four tasks against one synthetic shop MCP server (bench/code_mode_shop.py),
each with one checkable answer: a fan-out aggregation over dozens of
customers, a count over a 100 KB log, a four-step lookup chain, and a single
lookup as the control. The same model runs every task in five arms:

  codex-native  Codex with the shop server configured as an MCP server: the
                model calls each tool as a native tool call.
  codex-mcpx    Codex with no MCP server; the shop is reachable through mcpx
                from the shell, and the workspace AGENTS.md holds mcpx's own
                skill text (`mcpx skill install`).
  codex-code    codex-mcpx plus one paragraph asking the model to combine
                calls in a script and print only what it needs.
  agent-mcpx    Agent with the same mcpx skill text, composed with --agents.
  agent-code    agent-mcpx plus the same paragraph.

Native versus mcpx compares the call styles inside one harness; the Codex
and Agent mcpx arms compare harnesses on the same call style. Each run gets
a fresh workspace outside any repository, a fresh HOME, its own mcpx state
and, for Codex, its own CODEX_HOME holding a copy of the login, so neither
harness reads the person's own AGENTS.md, skills, MCP servers or mcpx
configuration. The arms of one task and trial start together.

Recorded per run: the answer and whether it is right, input (and cached
input) and output tokens, tool calls and the shell commands, and wall time.

Real model, real spend. Run it on the ChatGPT plan with Codex's login
(`codex login`), naming the model as Codex's /model picker shows it:

    .local/venv/bin/python -m bench.code_mode_eval --model gpt-6-luna \\
        --trials 3 --out .local/code-mode-eval/gpt-6-luna.json
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.code_mode_shop import ANSWER_FORMAT, TASKS, answer_of, expected  # noqa: E402

SHOP = Path(__file__).resolve().with_name('code_mode_shop.py')
ARMS = ('codex-native', 'codex-mcpx', 'codex-code', 'agent-mcpx', 'agent-code')
AGENT_TOOLS = 'shell,read,write,edit,wait,history'
CODE_HINT = ('When a task needs several tool calls or a large tool output, write one short script (shell, '
             'Python or jq) that makes the mcpx calls and prints only what you need, instead of calling '
             'tools one at a time and reading every result.')
_auth_lock = threading.Lock()


def _text(value):
    return value.decode(errors='replace') if isinstance(value, bytes) else (value or '')


def prompt(task):
    return f'{TASKS[task]}\n\nEnd your reply with one line: {ANSWER_FORMAT[task]}'


def mcpx_skill(mcpx):
    """mcpx's own skill text, as `mcpx skill install` writes it."""
    home = Path(tempfile.mkdtemp(prefix='code-mode-skill-'))
    try:
        subprocess.run([mcpx, 'skill', 'install'], env=dict(os.environ, HOME=str(home)), check=True,
                       capture_output=True)
        return (home / '.agents/skills/mcpx/SKILL.md').read_text()
    finally:
        shutil.rmtree(home, ignore_errors=True)


def guidance(arm, skill):
    if arm == 'codex-native':
        return None
    text = skill.split('\n---\n', 1)[-1].strip() if skill.startswith('---') else skill
    return text + ('\n\n' + CODE_HINT if arm.endswith('-code') else '') + '\n'


def isolated(run_dir, mcpx, with_shop):
    """Environment for one run: its own HOME and mcpx state, and mcpx on PATH."""
    home = run_dir / 'home'
    config = run_dir / 'xdg/config/mcpx'
    config.mkdir(parents=True)
    home.mkdir()
    servers = (f'[servers.shop]\ncommand = "{sys.executable}"\nargs = ["{SHOP}"]\n' if with_shop else '')
    # No fallback discovery: never import the person's Claude or Codex servers.
    (config / 'config.toml').write_text('fallback_sources = []\n' + servers)
    runtime = Path(tempfile.mkdtemp(prefix='cm-', dir='/tmp'))  # Unix socket paths are length-limited
    env = dict(os.environ, HOME=str(home), XDG_CONFIG_HOME=str(run_dir / 'xdg/config'),
               XDG_CACHE_HOME=str(run_dir / 'xdg/cache'), XDG_STATE_HOME=str(run_dir / 'xdg/state'),
               XDG_RUNTIME_DIR=str(runtime))
    env['PATH'] = str(Path(shutil.which(mcpx)).parent) + os.pathsep + env['PATH']
    for name in ('AGENTS_MD', 'CLAUDE_CONFIG_DIR'):
        env.pop(name, None)
    return env, runtime


def run_codex(arm, task, work, run_dir, env, args):
    codex_home = run_dir / 'codex'
    codex_home.mkdir()
    real = Path(os.environ.get('CODEX_HOME', Path.home() / '.codex')) / 'auth.json'
    shutil.copy2(real, codex_home / 'auth.json')
    config = [f'model = "{args.model}"', f'model_reasoning_effort = "{args.reasoning}"', *args.codex_config]
    if arm == 'codex-native':
        config += ['[mcp_servers.shop]', f'command = "{sys.executable}"', f'args = ["{SHOP}"]']
    (codex_home / 'config.toml').write_text('\n'.join(config) + '\n')
    last = run_dir / 'last.txt'
    command = [args.codex, 'exec', '--json', '--skip-git-repo-check', '--dangerously-bypass-approvals-and-sandbox',
               '-C', str(work), '-o', str(last), prompt(task)]
    started = time.monotonic()
    try:
        done = subprocess.run(command, env=dict(env, CODEX_HOME=str(codex_home)), capture_output=True, text=True,
                              timeout=args.timeout, cwd=work, stdin=subprocess.DEVNULL)
        stdout, status = done.stdout, 'completed' if done.returncode == 0 else f'exit {done.returncode}'
        stderr = done.stderr
    except subprocess.TimeoutExpired as error:
        stdout, status, stderr = _text(error.stdout), 'timeout', _text(error.stderr)
    wall = time.monotonic() - started
    # A refreshed login must reach the person's own file, or their next
    # refresh uses a token this copy already spent.
    with _auth_lock:
        copy = codex_home / 'auth.json'
        if copy.stat().st_mtime > real.stat().st_mtime and copy.read_bytes() != real.read_bytes():
            shutil.copy2(copy, real)
    (run_dir / 'events.jsonl').write_text(stdout)
    (run_dir / 'stderr.txt').write_text(stderr[-20000:])
    usage = {'input_tokens': 0, 'cached_input_tokens': 0, 'output_tokens': 0}
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get('type') == 'turn.completed':
            for key in usage:
                usage[key] += (event.get('usage') or {}).get(key, 0) or 0
    # `exec --json` does not report native MCP calls, so tools and model
    # requests come from the session's own record.
    tools, commands, requests = {}, [], 0
    for rollout in sorted(codex_home.glob('sessions/**/*.jsonl')):
        for line in rollout.read_text().splitlines():
            record = json.loads(line)
            payload = record.get('payload') or {}
            if record.get('type') == 'token_usage_record':
                requests += 1
            if record.get('type') != 'response_item' or payload.get('type') not in ('function_call', 'custom_tool_call'):
                continue
            name = payload.get('name', '?')
            if payload.get('namespace'):
                name = f"{payload['namespace']}.{name}"
            tools[name] = tools.get(name, 0) + 1
            if name == 'exec_command':
                try:
                    commands.append(str(json.loads(payload.get('arguments') or '{}').get('cmd', ''))[:300])
                except json.JSONDecodeError:
                    commands.append(str(payload.get('arguments'))[:300])
        shutil.copy2(rollout, run_dir / 'rollout.jsonl')
    answer = last.read_text() if last.exists() else ''
    return dict(status=status, answer=answer, usage=usage, tools=tools, commands=commands,
                model_requests=requests, wall_s=round(wall, 1))


def run_agent(arm, task, work, run_dir, env, args):
    socket = Path(tempfile.mkdtemp(prefix='cm-', dir='/tmp')) / 'agent.sock'
    store = run_dir / 'store.sqlite'
    daemon_env = dict(env, CODEX_HOME=os.environ.get('CODEX_HOME', str(Path.home() / '.codex')))
    log = open(run_dir / 'daemon.log', 'w')
    daemon = subprocess.Popen([args.agent, 'serve', '--store', str(store), '--socket', str(socket),
                               '--provider', args.provider], env=daemon_env, stdout=subprocess.DEVNULL, stderr=log)
    try:
        for _ in range(200):
            if socket.exists():
                break
            time.sleep(0.05)
        command = [args.agent, 'run', '--socket', str(socket), '--store', str(store), '--no-spawn', '--new',
                   '--bot', 'task', '--agents', '--model', f"{args.provider.split('=')[0]}/{args.model}", '--reasoning', args.reasoning,
                   '--tools', AGENT_TOOLS, '--workspace', str(work), '--', prompt(task)]
        started = time.monotonic()
        try:
            done = subprocess.run(command, env=env, capture_output=True, text=True, timeout=args.timeout, cwd=work,
                                  stdin=subprocess.DEVNULL)
            stdout, stderr = done.stdout, done.stderr
        except subprocess.TimeoutExpired as error:
            stdout, stderr = _text(error.stdout), 'timeout'
        wall = time.monotonic() - started
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
        log.close()
        shutil.rmtree(socket.parent, ignore_errors=True)
    (run_dir / 'events.jsonl').write_text(stdout)
    (run_dir / 'stderr.txt').write_text(stderr[-20000:])
    usage = {'input_tokens': 0, 'cached_input_tokens': 0, 'output_tokens': 0}
    tools, commands, text, status, requests = {}, [], [], 'timeout', 0
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind, data = event.get('event'), event.get('data') or {}
        if kind == 'usage':
            requests += 1
            for key in usage:
                usage[key] += data.get(key, 0) or 0
        elif kind == 'tool_started':
            tools[data.get('name')] = tools.get(data.get('name'), 0) + 1
            if data.get('name') == 'shell':
                try:
                    commands.append(json.loads(data.get('arguments', '{}')).get('command', '')[:300])
                except json.JSONDecodeError:
                    commands.append(str(data.get('arguments', ''))[:300])
        elif kind == 'text_delta':
            text.append(event.get('text', ''))
        elif kind == 'message':
            text.append('\n')
        elif kind == 'turn_finished':
            status = data.get('status')
    return dict(status=status, answer=''.join(text), usage=usage, tools=tools, commands=commands,
                model_requests=requests, wall_s=round(wall, 1))


def run_one(arm, task, trial, root, args, skill):
    run_dir = root / f'{task}-{trial}-{arm}'
    run_dir.mkdir(parents=True)
    work = Path(tempfile.mkdtemp(prefix=f'code-mode-{task}-{arm}-'))  # outside any repository's AGENTS.md
    text = guidance(arm, skill)
    if text:
        (work / 'AGENTS.md').write_text(text)
    env, runtime = isolated(run_dir, args.mcpx, with_shop=arm != 'codex-native')
    try:
        runner = run_codex if arm.startswith('codex') else run_agent
        record = runner(arm, task, work, run_dir, env, args)
    except Exception as error:  # a broken run is recorded, never dropped
        record = dict(status=f'error: {error}', answer='', usage={}, tools={}, commands=[], model_requests=None,
                      wall_s=None)
    finally:
        # mcpx's daemon and the shop server exit on their own after mcpx's idle keepalive.
        shutil.rmtree(runtime, ignore_errors=True)
        shutil.rmtree(work, ignore_errors=True)
    got = answer_of(record['answer'])
    record.update(arm=arm, task=task, trial=trial, got=got, want=expected()[task],
                  correct=got is not None and got.lower() == expected()[task].lower(),
                  tool_calls=sum(record['tools'].values()), answer=record['answer'][-2000:])
    return record


def summarize(records):
    rows = []
    for arm in ARMS:
        for task in TASKS:
            mine = [r for r in records if r['arm'] == arm and r['task'] == task]
            if not mine:
                continue

            def med(values):
                values = [v for v in values if v is not None]
                return statistics.median(values) if values else None
            rows.append(dict(arm=arm, task=task, correct=f"{sum(r['correct'] for r in mine)}/{len(mine)}",
                             input_tokens=med([r['usage'].get('input_tokens') for r in mine]),
                             cached_input_tokens=med([r['usage'].get('cached_input_tokens') for r in mine]),
                             output_tokens=med([r['usage'].get('output_tokens') for r in mine]),
                             tool_calls=med([r['tool_calls'] for r in mine]),
                             wall_s=med([r['wall_s'] for r in mine])))
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    parser.add_argument('--model', required=True, help="the model id Codex's /model picker shows, e.g. gpt-6-luna")
    parser.add_argument('--reasoning', default='high')
    parser.add_argument('--trials', type=int, default=3)
    parser.add_argument('--tasks', default=','.join(TASKS))
    parser.add_argument('--arms', default=','.join(ARMS))
    parser.add_argument('--timeout', type=int, default=900, help='seconds per run')
    parser.add_argument('--agent', default='.local/target/release/agent')
    parser.add_argument('--codex', default='codex')
    parser.add_argument('--mcpx', default='mcpx')
    parser.add_argument('--provider', default='chatgpt', help="Agent's provider SPEC; default the ChatGPT plan")
    parser.add_argument('--codex-config', action='append', default=[],
                        help="an extra line for each Codex run's config.toml (tests point Codex at a local model)")
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    args.agent = str(Path(args.agent).resolve())
    arms, tasks = args.arms.split(','), args.tasks.split(',')
    for name in arms:
        if name not in ARMS:
            parser.error(f'unknown arm {name}')
    root = args.out.with_suffix('')
    root.mkdir(parents=True, exist_ok=False)
    skill = mcpx_skill(args.mcpx)
    versions = {'codex': subprocess.run([args.codex, '--version'], capture_output=True, text=True).stdout.strip(),
                'mcpx': subprocess.run([args.mcpx, '--version'], capture_output=True, text=True).stdout.strip(),
                'agent': subprocess.run([args.agent, '--version'], capture_output=True, text=True).stdout.strip()}
    records = []
    started = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
    for trial in range(args.trials):
        for task in tasks:
            results = {}

            def go(arm):
                results[arm] = run_one(arm, task, trial, root, args, skill)
            threads = [threading.Thread(target=go, args=(arm,)) for arm in arms]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            for arm in arms:
                r = results[arm]
                records.append(r)
                print(json.dumps({k: r[k] for k in ('arm', 'task', 'trial', 'status', 'correct', 'got', 'tool_calls',
                                                     'wall_s')} | {'usage': r['usage']}), flush=True)
            args.out.write_text(json.dumps({'model': args.model, 'reasoning': args.reasoning, 'versions': versions,
                                            'started': started, 'summary': summarize(records),
                                            'runs': records}, indent=1))
    for row in summarize(records):
        print(json.dumps(row))


if __name__ == '__main__':
    main()
