#!/usr/bin/env python3
"""Offline playground for agent-tui: a synthetic streaming Responses model
plus a daemon bound to it, so bots, delegation, forks, waits, interrupts, and
attach/detach can be exercised with no provider credentials.

    python3 tui/playground.py [--root DIR] [--agent PATH] [--no-tui]

By default it opens agent-tui in this terminal once the daemon is ready and
stops the daemon when the TUI exits (^d). Rerunning resumes the same bot. With --no-tui it stays in the foreground and prints the attach
command for another terminal.

The model streams a reply word by word. Prompt prefixes drive behavior:

    shell: CMD        one shell tool call, then a reply summarizing its output
    bg: CMD           the same command in the background, then wait on its proc handle
    delegate: NAME    run `agent run --new --bot NAME ...` in the shell tool
                      and then wait on that turn (parent shows ⏳ NAME/1)
    fanout: A,B,C     delegate to several bots, detached, and wait --any
    slow: TEXT        stream the reply slowly (a long turn to interrupt)
    md:               a reply with a code fence, bold and inline code
    hold:             keep the model call open until the server is released
                      (Enter in --no-tui mode, `kill -USR1 PID`, or 30 seconds)
    limited:          always 429 with Retry-After 1 (turn_paced)
    anything else     a short streamed reply that echoes the prompt

Only the synthetic HTTP loopback and the local store are touched.
"""
import argparse
import http.server
import json
import os
import shlex
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path


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
        request = json.loads(self.rfile.read(length))
        self.server.requests += 1
        items = request['input']
        user = [i for i in items if i.get('role') == 'user'][-1]['content'][0]['text']
        last = items[-1]
        after_tool = last.get('type') == 'function_call_output'
        if user.startswith('limited:'):
            body = json.dumps({'error': {'message': 'playground rate limit'}}).encode()
            self.send_response(429)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Retry-After', '1')
            self.end_headers()
            self.wfile.write(body)
            return
        pace = 0.04
        calls = []
        if after_tool:
            output = last.get('output', '')
            try:
                shell = json.loads(output)
                stdout = shell.get('stdout', '') if isinstance(shell, dict) else output
            except ValueError:
                stdout = output
            preview = stdout.strip().splitlines()[:3]
            text = 'Tool finished. ' + (' / '.join(preview) if preview else 'No output.')
        elif user.startswith('shell:'):
            text, calls = '', [('shell', {'command': user[6:].strip(), 'timeout_ms': 60000})]
        elif user.startswith('bg:'):
            text, calls = '', [('shell', {'command': user[3:].strip(), 'timeout_ms': 60000, 'background': True})]
        elif user.startswith('delegate:'):
            name = user[9:].strip()
            command = (f'"$AGENT_BIN" run --new --bot {shlex.quote(name)} --model "$AGENT_MODEL" --detach '
                       f'{shlex.quote("slow: I am " + name + ", working on the delegated task.")}')
            text, calls = '', [('shell', {'command': command, 'timeout_ms': 60000})]
        elif user.startswith('fanout:'):
            names = [n.strip() for n in user[7:].split(',') if n.strip()]
            parts = [f'"$AGENT_BIN" run --new --bot {shlex.quote(n)} --model "$AGENT_MODEL" --detach '
                     f'{shlex.quote("slow: " + n + " reporting for duty.")}' for n in names]
            text, calls = '', [('shell', {'command': ' && '.join(parts), 'timeout_ms': 60000})]
        elif user.startswith('slow:'):
            text, pace = 'Streaming slowly: ' + user[5:].strip() + ' ' + ' '.join(f'w{i}' for i in range(40)), 0.25
        elif user.startswith('hold:'):
            text = 'Released.'
        elif user.startswith('md:'):
            text = ('Exactly. The CLI supports **peers** through the shell tool:\n\n```sh\n'
                    'agent run --detach --new --bot scout -- "Inspect the workspace"\n```\n\n'
                    'Collect it with `wait` on the printed handle. Nothing was launched yet.')
        else:
            text = f'You said: {user.strip()}. Reasoning done; nothing else to do.'
        # A shell result that launched delegates: park on their turns.
        if after_tool:
            handles = []
            try:
                direct = json.loads(output)
                if isinstance(direct, dict) and str(direct.get('handle', '')).startswith('proc:'):
                    handles.append(direct['handle'])
            except ValueError:
                pass
            if '"handle"' in stdout:
                for line in stdout.splitlines():
                    try:
                        handles.append(json.loads(line)['handle'])
                    except (ValueError, KeyError):
                        continue
            if handles:
                text, calls = '', [('wait', {'handles': handles, 'any': len(handles) > 1})]
        output = []
        for index, (name, arguments) in enumerate(calls):
            output.append({'type': 'function_call', 'name': name, 'call_id': f'call-{self.server.requests}-{index}',
                           'arguments': json.dumps(arguments)})
        if text:
            output.append({'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': text}]})
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()
        self.chunk({'type': 'response.created', 'response': {'id': f'r{self.server.requests}'}})
        if user.startswith('hold:') and not after_tool:
            for _ in range(15):
                if self.server.release.wait(2):
                    break
                self.raw(b': hold\n\n')
        # Streamed deltas must reassemble to the final text exactly, spaces,
        # runs of spaces and newlines included.
        words = text.split(' ')
        for index, word in enumerate(words):
            delta = word + (' ' if index + 1 < len(words) else '')
            if not delta:
                continue
            self.chunk({'type': 'response.output_text.delta', 'delta': delta})
            if word:
                time.sleep(pace)
        usage = {'input_tokens': max(1, length // 4), 'output_tokens': max(1, len(text) // 4),
                 'input_tokens_details': {'cached_tokens': 0}}
        self.chunk({'type': 'response.completed', 'response': {'status': 'completed', 'output': output, 'usage': usage}})
        self.raw(b'')
        self.wfile.write(b'0\r\n\r\n')
        self.wfile.flush()

    def chunk(self, event):
        self.raw(('data: ' + json.dumps(event) + '\n\n').encode())

    def raw(self, data):
        if data:
            self.wfile.write(f'{len(data):x}\r\n'.encode() + data + b'\r\n')
            self.wfile.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--root', default='.local/playground', help='store and socket directory')
    parser.add_argument('--agent', default='.local/target/release/agent', help='agent binary')
    parser.add_argument('--max-active', type=int, default=64)
    parser.add_argument('--no-tui', action='store_true', help='do not launch agent-tui; print the attach command')
    parser.add_argument('--tui', default=None, help='agent-tui binary (default: release, then debug build)')
    args = parser.parse_args()
    tui = args.tui or next((p for p in ('.local/target/release/agent-tui', '.local/target/debug/agent-tui')
                            if Path(p).exists()), None)
    if not args.no_tui and tui is None:
        print('no agent-tui binary; run `cargo build --release -p agent-tui` or pass --no-tui', file=sys.stderr)
        return 2
    root = Path(args.root).resolve()
    root.mkdir(parents=True, exist_ok=True)
    (root / 'workspace').mkdir(exist_ok=True)

    class Server(http.server.ThreadingHTTPServer):
        daemon_threads = True
        request_queue_size = 1024
    server = Server(('127.0.0.1', 0), Model)
    server.requests = 0
    server.release = threading.Event()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f'http://127.0.0.1:{server.server_port}/v1'

    store = root / 'state.sqlite'
    socket = root / 'state.sqlite.sock'
    if len(str(socket).encode()) > 96:
        # Unix socket paths are short (104 bytes on macOS); the daemon uses
        # the path as given, so pick a short one for deep checkouts.
        short = Path(f'/tmp/agent-play-{os.getuid()}')
        short.mkdir(mode=0o700, exist_ok=True)
        import hashlib
        socket = short / f'{hashlib.sha1(str(root).encode()).hexdigest()[:8]}.sock'
    env = {k: v for k, v in os.environ.items() if not k.endswith('_API_KEY')}
    daemon = subprocess.Popen(
        [args.agent, 'serve', '--store', str(store), '--socket', str(socket),
         '--provider', f'openai=responses,{url}', '--max-active', str(args.max_active)],
        env=env, stdout=subprocess.PIPE, text=True)

    def stop():
        # Every exit path, including a Ctrl-C before ready, ends the daemon.
        if daemon.poll() is None:
            daemon.send_signal(signal.SIGTERM)
            try:
                daemon.wait(timeout=8)
            except subprocess.TimeoutExpired:
                daemon.kill()
        server.shutdown()

    def release(*_):
        server.release.set()
        server.release = threading.Event()

    try:
        return run(args, tui, root, socket, env, daemon, server, url, stop, release)
    finally:
        stop()


def run(args, tui, root, socket, env, daemon, server, url, stop, release):
    ready = daemon.stdout.readline().strip()
    try:
        banner = json.loads(ready)
    except ValueError:
        banner = {}
    if banner.get('event') != 'ready':
        code = daemon.wait()
        print(f'daemon exited with status {code} before ready', file=sys.stderr)
        print('a daemon may already own this store: check `pgrep -fl "agent serve"` and stop it, '
              'or pass a different --root', file=sys.stderr)
        return 1
    workspace = root / 'workspace'
    attach = f'AGENT_MODEL=openai/play {tui or ".local/target/release/agent-tui"} --socket {socket} --workspace {workspace}'
    print(f'model   {url}')
    print(f'daemon  pid {daemon.pid}  protocol {banner.get("protocol")}  socket {socket}')
    print(f'attach  {attach}')
    print(f'cli     {args.agent} run --socket {socket} --new --bot Bob --model openai/play --workspace {workspace} "hello"')
    signal.signal(signal.SIGUSR1, release)

    def interrupted(*_):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    if args.no_tui:
        print('Ctrl-C stops the daemon; the store persists for the next run. Enter releases hold: prompts.')
        while daemon.poll() is None:
            line = sys.stdin.readline()
            if not line:
                daemon.wait()
                break
            release()
            print('released held prompts')
    else:
        print('opening agent-tui; ^d detaches and stops the playground daemon. Store persists for the next run.')
        time.sleep(0.5)
        child = subprocess.Popen([tui, '--socket', str(socket), '--workspace', str(workspace)],
                                 env=dict(env, AGENT_MODEL='openai/play'))
        # Ctrl-C inside the TUI is the TUI's to handle.
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        try:
            code = child.wait()
        finally:
            signal.signal(signal.SIGINT, signal.default_int_handler)
        if code != 0:
            print(f'agent-tui exited with status {code}', file=sys.stderr)
    if daemon.poll() is not None:
        print(f'daemon exited with status {daemon.returncode}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main() or 0)
    except KeyboardInterrupt:
        sys.exit(0)
