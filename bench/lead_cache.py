"""Lead cache: does a busy Sonny lead's request keep its prefix from one
model call to the next, so a provider's prompt cache can reuse it?

A synthetic busy lead, no spend and nothing real: one `sonny acp` lead in a
fake HOME, code mode as the build ships it, a source whose news the lookout
brings every few seconds, a scripted Responses model that answers most notes
with nothing and some with a code-mode script, and a context window small
enough that the lead compacts a few times. The model records every request
the daemon sends and reports the cached tokens a prefix cache would give
(the longest prefix the request shares with any earlier one under the same
`prompt_cache_key`, in whole 128-token blocks past the first 1,024), so
`agent usage` reads as a provider's would.

    python3 -m bench.lead_cache run --bin DIR --out OUT [--minutes 5] [--saves S] [--edit-rule M]
    python3 -m bench.lead_cache diff OUT [--show 5]

`diff` compares each request of a bot with the bot's previous one: where
the first byte moves (the tools, the instructions, or which input item), how
much of the request a prefix cache could reuse, and the bytes it could not.
Content is synthetic; nothing names a real person, company or machine.
"""
import argparse
import fcntl
import http.server
import json
import os
import random
import pty
import shutil
import struct
import subprocess
import termios
import sys
import threading
import time
from pathlib import Path

from . import role_eval

# Bytes per token, as the daemon estimates them.
BYTES_PER_TOKEN = 4
# A prefix cache's grain: whole blocks past a minimum, as OpenAI's prompt
# caching documents (1,024 tokens, then 128 at a time).
CACHE_MIN, CACHE_BLOCK = 1024, 128

SOURCE = r'''import json, sys, time
from pathlib import Path
root = Path(sys.argv[1])
count = root / 'count'
n = int(count.read_text() or 0) + 1 if count.exists() else 1
count.write_text(str(n))
words = ('build deploy review branch release ticket metric alert queue cache shard '
         'replica index schema config rollout canary latency budget owner').split()
for i in range(max(1, n - 2), n + 1):
    body = ' '.join(words[(i * 7 + k) % len(words)] for k in range(160))
    print(json.dumps({'id': f'e{i}', 'from': f'bot{i % 5}', 'topic': f'topic{i % 3}',
                      'text': f'Event {i}: {body}'}))
'''


# --- the model -----------------------------------------------------------------

def canonical(request):
    """The request as a prefix cache reads it, in order: tools, then the
    instructions, then each input item; with where each part starts."""
    parts = [('tools', json.dumps(request.get('tools', []), sort_keys=False, separators=(',', ':'))),
             ('instructions', request.get('instructions') or '')]
    for index, item in enumerate(request.get('input') or []):
        parts.append((f'input[{index}]', json.dumps(item, separators=(',', ':'))))
    return parts


def joined(parts):
    return ''.join(text for _, text in parts)


def common(a, b):
    """The length of the prefix `a` and `b` share."""
    n = min(len(a), len(b))
    lo, hi = 0, n
    # Binary search on equal prefixes: compare slices, not characters.
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if a[:mid] == b[:mid]:
            lo = mid
        else:
            hi = mid - 1
    return lo


def cached_tokens(shared_bytes):
    tokens = shared_bytes // BYTES_PER_TOKEN
    if tokens < CACHE_MIN:
        return 0
    return tokens - (tokens - CACHE_MIN) % CACHE_BLOCK


class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def handle(self):
        try:
            super().handle()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_POST(self):
        length = int(self.headers['Content-Length'])
        raw = self.rfile.read(length)
        request = json.loads(raw)
        server = self.server
        key = request.get('prompt_cache_key') or ''
        text = joined(canonical(request))
        with server.lock:
            seen = server.seen.setdefault(key, [])
            shared = max((common(text, earlier) for earlier in seen), default=0)
            seen.append(text)
            del seen[:-16]
            server.count += 1
            number = server.count
            with open(server.out / 'requests.jsonl', 'a') as log:
                log.write(json.dumps({'n': number, 't': time.time(), 'key': key, 'bytes': length,
                                      'shared': shared, 'request': request}) + '\n')
        output, reply = self.script(request)
        input_tokens = len(text) // BYTES_PER_TOKEN
        usage = {'input_tokens': input_tokens, 'output_tokens': 20,
                 'input_tokens_details': {'cached_tokens': min(cached_tokens(shared), input_tokens)}}
        events = [{'type': 'response.created', 'response': {'id': f'resp_{number}'}},
                  *([{'type': 'response.output_text.delta', 'delta': reply}] if reply else []),
                  {'type': 'response.completed', 'response': {'id': f'resp_{number}', 'status': 'completed',
                                                              'output': output, 'usage': usage}}]
        frames = b''.join(('data: ' + json.dumps(e) + '\n\n').encode() for e in events)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(frames)))
        self.end_headers()
        self.wfile.write(frames)
        self.wfile.flush()

    def script(self, request):
        """What the model answers: a summary to a compaction request, a
        reviewer's verdict, a follow on the first prompt, a script on every
        third note, and nothing on the rest, as a busy lead mostly does."""
        server = self.server
        items = request.get('input') or []
        last = items[-1] if items else {}
        users = [i for i in items if i.get('role') == 'user']
        said = ''
        if users:
            content = users[-1].get('content')
            said = content if isinstance(content, str) else ''.join(
                c.get('text', '') for c in content or [] if isinstance(c, dict))
        number = server.count
        reasoning = {'type': 'reasoning', 'id': f'rs_{number}', 'summary': [],
                     'encrypted_content': f'enc{number}-' + 'r' * 400}

        def message(text):
            return [reasoning, {'type': 'message', 'role': 'assistant', 'id': f'msg_{number}',
                                'status': 'completed', 'content': [{'type': 'output_text', 'text': text,
                                                                    'annotations': []}]}], text

        def call(name, arguments):
            # A tool the request declares as custom takes raw input; a
            # function takes JSON arguments.
            tools = request.get('tools') or []
            flat = []
            for tool in tools:
                flat.extend(tool.get('tools') or [tool])
            kind = next((t.get('type') for t in flat if t.get('name') == name), 'function')
            if kind == 'custom':
                item = {'type': 'custom_tool_call', 'id': f'ctc_{number}', 'call_id': f'call_{number}',
                        'name': name, 'input': arguments['input'], 'status': 'completed'}
            else:
                item = {'type': 'function_call', 'id': f'fc_{number}', 'call_id': f'call_{number}',
                        'name': name, 'arguments': json.dumps(arguments), 'status': 'completed'}
            return [reasoning, item], ''

        def names():
            out = []
            for tool in request.get('tools') or []:
                out.extend(t.get('name') for t in (tool.get('tools') or [tool]))
            return out

        if said.startswith('[compaction request]'):
            return message('Summary of the work so far. ' + 'The lead watched the feed and answered. ' * 60)
        if said.startswith('{"state"'):
            verdict = json.dumps({'assessments': [
                {'id': id, 'risk_level': 'low', 'user_authorization': 'unknown', 'outcome': 'allow',
                 'rationale': ''} for id in json.loads(said)['assess']]})
            return [{'type': 'message', 'role': 'assistant', 'content': [
                {'type': 'output_text', 'text': verdict}]}], verdict
        if last.get('type') in ('function_call_output', 'custom_tool_call_output'):
            # After a call: the turn ends on a short reply, or on nothing.
            return message('' if number % 2 else 'Checked; nothing for the person.')
        if 'SUBSCRIBE' in said:
            add = {'add': {'name': 'feed', 'sources': ['feed']}}
            if 'follows' in names():
                return call('follows', add)
            return call('exec', {'input': f'text(JSON.stringify(await tools.follows({json.dumps(add)})));'})
        lead = request.get('prompt_cache_key', '').endswith('-1')
        if not lead:
            # A task: one slow command, then its turn ends with a reply.
            if 'exec' in names():
                return call('exec', {'input': 'const r = await tools.shell({command: "sleep 3; ls"});\n'
                                              'text(r.content.map(c => c.text).join("\\n"));'})
            return call('shell', {'command': 'sleep 3; ls'})
        server.notes += 1
        if server.notes % 7 == 1 and server.tasks < server.max_tasks:
            server.tasks += 1
            return call('start_task', {'name': f'job{server.tasks}', 'title': f'Job {server.tasks}',
                                       'brief': f'Look over the repository and list its files (job {server.tasks}).'})
        if server.notes % 3 == 0:
            # A script slow enough that the next note lands while it runs.
            script = ('const r = await tools.shell({command: "sleep 4; git log --oneline -3"});\n'
                      'text(r.content.map(c => c.text).join("\\n"));')
            if 'exec' in names():
                return call('exec', {'input': script})
            return call('shell', {'command': 'sleep 4; git log --oneline -3'})
        return message('')


class Window:
    """A Sonny window on the lead, in a terminal of its own, as the person
    keeps one open: it reads the screen and keys nothing."""

    def __init__(self, sonny, env, cwd, log):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 50, 160, 0, 0))
        self.master = master
        self.p = subprocess.Popen([str(sonny)], env={**env, 'TERM': 'xterm-256color'}, cwd=cwd,
                                  stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
        os.close(slave)
        self.log = open(log, 'wb')
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        while True:
            try:
                data = os.read(self.master, 65536)
            except OSError:
                return
            if not data:
                return
            self.log.write(data)
            self.log.flush()

    def close(self):
        if self.p.poll() is None:
            self.p.terminate()
            try:
                self.p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.p.kill()


def start_model(out):
    class Server(http.server.ThreadingHTTPServer):
        daemon_threads = True
        request_queue_size = 256
    server = Server(('127.0.0.1', 0), Model)
    server.out = Path(out)
    server.lock = threading.Lock()
    server.seen, server.count, server.notes, server.tasks = {}, 0, 0, 0
    server.max_tasks = int(os.environ.get('LEAD_CACHE_TASKS', '6'))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f'http://127.0.0.1:{server.server_port}/v1'


# --- one run -------------------------------------------------------------------

def run(bin_dir, out, minutes, every, window, saves, open_window, edit_rule=None):
    out = Path(out).resolve()
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    server, url = start_model(out)
    root = Path.home() / '.lead-cache' / out.name
    if root.exists():
        shutil.rmtree(root)
    home, repo = root / 'h', root / 'shop'
    (home / '.agent').mkdir(parents=True)
    (root / 'gitconfig').write_text('[user]\n\tname = Eval\n\temail = eval@example.com\n'
                                    '[init]\n\tdefaultBranch = main\n')
    (root / 'source.py').write_text(SOURCE)
    own = root / 'bin'
    own.mkdir()
    for name in ('agent', 'sonny'):
        shutil.copy2(Path(bin_dir) / name, own / name)
    model = 'syn/sol-synthetic'
    (home / '.agent/config.toml').write_text(f'''model = "{model}"
provider = "syn=responses,{url}"

[reviewer]
model = "{model}"

[sonny]
default_permissions = "full"
suggest = false

[sonny.sources.feed]
command = "{sys.executable} {root}/source.py {root}"
every = "{every}s"

[context_windows]
"{model}" = {window}
''')
    env = {'HOME': str(home), 'PATH': f'{own}:/usr/bin:/bin:/usr/sbin:/sbin',
           'GIT_CONFIG_GLOBAL': str(root / 'gitconfig'), 'AGENT_BIN': str(own / 'agent'),
           'LANG': 'en_US.UTF-8', 'TERM': 'dumb'}
    for path, text in role_eval.FILES.items():
        (repo / path).parent.mkdir(parents=True, exist_ok=True)
        (repo / path).write_text(text)
    (repo / '.agents').mkdir(exist_ok=True)
    (repo / '.agents/project.toml').write_text('name = "shop"\n')
    for args in (['git', 'init', '-q'], ['git', 'add', '-A'], ['git', 'commit', '-qm', 'shop']):
        role_eval.sh(args, cwd=repo, env=env)
    agent, sonny = own / 'agent', own / 'sonny'
    role_eval.sh([str(agent), 'start', '--idle-exit', '0'], env=env)
    acp = shown = None
    try:
        acp = role_eval.Acp(sonny, env, repo, out / 'acp.jsonl', home)
        acp.ask('initialize', role_eval.hello('lead-cache'))
        session = acp.ask('session/new', {'cwd': str(repo), 'mcpServers': []})['result']['sessionId']
        acp.ask('session/prompt', {'sessionId': session, 'prompt': [
            {'type': 'text', 'text': 'SUBSCRIBE to the feed source and steer on each event.'}]})
        if open_window:
            shown = Window(sonny, env, repo, out / 'window.log')
        # Facts other agents of the project save, as they would through the
        # memory tool: each adds a line to the project's memory index.
        index = home / '.agents/memory/projects/shop/MEMORY.md'
        index.parent.mkdir(parents=True, exist_ok=True)
        started = time.time()
        deadline = started + minutes * 60
        saved, last = 0, time.time()
        while time.time() < deadline:
            time.sleep(1)
            # The person edits the folder's rules once: the lead takes the
            # new text at a turn's end, one call that reads no cache.
            if edit_rule is not None and time.time() - started >= edit_rule * 60:
                edit_rule = None
                with open(repo / 'AGENTS.md', 'a') as f:
                    f.write('Keep each commit to one change.\n')
            if saves and time.time() - last >= saves:
                saved, last = saved + 1, time.time()
                with open(index, 'a') as f:
                    f.write(f'- fact{saved}.md: a synthetic fact saved by a task, number {saved}\n')
    finally:
        usage = subprocess.run([str(agent), 'usage', '--name', 'shop.*'], env=env, capture_output=True, text=True)
        (out / 'usage.json').write_text(usage.stdout + usage.stderr)
        bots = subprocess.run([str(agent), 'ls'], env=env, capture_output=True, text=True)
        (out / 'bots.txt').write_text(bots.stdout)
        # The CPU each of the build's processes used, the tool host's too.
        procs = subprocess.run(['ps', '-eo', 'pid,time,rss,args'], capture_output=True, text=True)
        (out / 'procs.txt').write_text(''.join(line + '\n' for line in procs.stdout.splitlines()
                                               if str(root) in line))
        if acp:
            acp.close()
        if shown:
            shown.close()
        subprocess.run([str(agent), 'shutdown'], env=env, capture_output=True)
        server.shutdown()
        # Its copy of the build and its store, which no report reads.
        shutil.rmtree(root, ignore_errors=True)
    print(out)


# --- the comparison --------------------------------------------------------------

def where(parts, at):
    """The part of the request that byte `at` falls in."""
    start = 0
    for name, text in parts:
        if at < start + len(text):
            return name, at - start
        start += len(text)
    return 'end', 0


def diff(out, show):
    rows = [json.loads(line) for line in (Path(out) / 'requests.jsonl').read_text().splitlines()]
    previous = {}
    totals = {}
    moves = {}
    shown = 0
    for row in rows:
        key, request = row['key'], row['request']
        parts = canonical(request)
        text = joined(parts)
        t = totals.setdefault(key, {'requests': 0, 'bytes': 0, 'reusable': 0, 'cached': 0, 'breaks': 0})
        t['requests'] += 1
        t['bytes'] += len(text)
        t['cached'] += cached_tokens(row['shared']) * BYTES_PER_TOKEN
        if key in previous:
            before_parts = previous[key]
            before = joined(before_parts)
            at = common(text, before)
            t['reusable'] += at
            if at < len(before):
                # The previous request is not a prefix of this one: the
                # part of this one where the first byte moved.
                part, offset = where(parts, at)
                kind = part.split('[')[0]
                moves.setdefault(key, {}).setdefault(kind, 0)
                moves[key][kind] += 1
                t['breaks'] += 1
                if shown < show:
                    shown += 1
                    print(f'#{row["n"]} {key}: moved in {part} at +{offset} of {len(text)} bytes '
                          f'(reusable {at / max(1, len(text)):.0%})')
                    print('  before:', repr(before[max(0, at - 80):at + 160]))
                    print('  now:   ', repr(text[max(0, at - 80):at + 160]))
        previous[key] = parts
    print()
    print(f'{"key":<48} {"calls":>6} {"MB sent":>8} {"reuse":>6} {"cached":>7} {"breaks":>6}  where')
    for key, t in totals.items():
        reuse = t['reusable'] / max(1, t['bytes'])
        cached = t['cached'] / max(1, t['bytes'])
        print(f'{key[-48:]:<48} {t["requests"]:>6} {t["bytes"] / 1e6:>8.2f} {reuse:>6.0%} {cached:>7.0%} '
              f'{t["breaks"]:>6}  {moves.get(key, {})}')
    return totals, moves


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='command', required=True)
    r = sub.add_parser('run')
    r.add_argument('--bin', required=True)
    r.add_argument('--out', required=True)
    r.add_argument('--minutes', type=float, default=5)
    r.add_argument('--every', type=int, default=3)
    r.add_argument('--window', type=int, default=60000)
    r.add_argument('--saves', type=float, default=0, help='seconds between memory saves; 0 for none')
    r.add_argument('--open-window', action='store_true', help='keep a Sonny window open on the lead')
    r.add_argument('--edit-rule', type=float, default=None, help='minutes in, the person edits AGENTS.md once')
    d = sub.add_parser('diff')
    d.add_argument('out')
    d.add_argument('--show', type=int, default=5)
    args = parser.parse_args()
    if args.command == 'run':
        run(args.bin, args.out, args.minutes, args.every, args.window, args.saves, args.open_window,
            args.edit_rule)
    else:
        diff(args.out, args.show)


if __name__ == '__main__':
    main()
