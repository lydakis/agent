"""Fixed model context against growing stored history.

Seeds one bot with N turns through the daemon and a synthetic Responses
provider, then measures what a caller pays as stored items grow while the
context window stays fixed: daemon startup and a separate resume operation, one turn's request
bytes and latency, a fork from the head and from the first checkpoint, a
`history` tool read of turn 1, and daemon RSS. No real provider is involved.

    .local/venv/bin/python -m bench.long_history --items 10000 --out .local/bench/history-10k
"""
import argparse
import http.server
import json
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

import psutil

from .runtime_client import Client
from .targets import file_hash

FILLER = 'x' * 200  # ~300 bytes per item once encoded, like short real turns


class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers['Content-Length'])
        request = json.loads(self.rfile.read(length))
        user = [i for i in request['input'] if i.get('role') == 'user'][-1]['content'][0]['text']
        last = request['input'][-1]
        self.server.seen.append({'bytes': length, 'items': len(request['input']),
                                 'note': request['input'][0]['content'][0]['text'][:40]
                                 if request['input'][0].get('role') == 'user' else None})
        if user.startswith('history:') and last.get('type') != 'function_call_output':
            text, output = '', [{'type': 'function_call', 'name': 'history', 'call_id': 'h-1',
                                 'arguments': json.dumps({'turn': int(user[8:])})}]
        elif last.get('type') == 'function_call_output':
            text = 'read:' + last['output'][:200]
            output = [{'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': text}]}]
        else:
            text = 'reply ' + FILLER
            output = [{'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': text}]}]
        events = [{'type': 'response.created', 'response': {'id': 'r'}},
                  *([{'type': 'response.output_text.delta', 'delta': text}] if text else []),
                  {'type': 'response.completed', 'response': {'status': 'completed', 'output': output,
                   'usage': {'input_tokens': 1, 'output_tokens': 1, 'input_tokens_details': {'cached_tokens': 0}}}}]
        body = b''.join(('data: ' + json.dumps(e) + '\n\n').encode() for e in events)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def timed(function):
    started = time.monotonic()
    result = function()
    return result, round((time.monotonic() - started) * 1000, 2)


def run(binary, out, items, context_bytes, context_items):
    store = out / 'state.sqlite'
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
    server.seen, server.daemon_threads = [], True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f'http://127.0.0.1:{server.server_port}/v1'
    extra = ('--context-bytes', str(context_bytes), '--context-items', str(context_items))

    def client():
        return Client(binary, store, url, 'echo,history', extra=extra)

    def turn(c, bot, request_id, prompt):
        turn_id = c.request('submit', bot=bot, request_id=request_id, prompt=prompt, workspace=str(out))['result']['turn']
        finished = c.receive(lambda m: m.get("event") == "turn_finished" and m.get("turn") == turn_id, timeout=60)
        assert finished['data']['status'] == 'completed', finished
        c.saved.clear()  # the controller keeps no per-turn events; this measures the daemon
        return turn_id

    c = client()
    c.request('create', bot='long', workspace=str(out))
    seeded_turns = items // 2
    started = time.monotonic()
    for n in range(seeded_turns):
        turn(c, 'long', f's{n}', f'seed {n} ' + FILLER)
    seed_seconds = round(time.monotonic() - started, 2)
    first_checkpoint = c.request('events', bot='long', after=0, limit=8)['result']['events']
    first_node = next(e['data']['node'] for e in first_checkpoint if e['event'] == 'message')
    c.close()
    server.seen.clear()
    # Measure readiness and the subsequent resume request separately.
    c, startup_ms = timed(client)
    head, resume_only_ms = timed(lambda: c.request('resume', bot='long')['result']['head'])
    daemon = psutil.Process(c.process.pid)
    rss_after_resume = daemon.memory_info().rss
    _, turn_ms = timed(lambda: turn(c, 'long', 'measured', 'measured turn ' + FILLER))
    request = server.seen[-1]
    rss_after_turn = daemon.memory_info().rss
    _, fork_head_ms = timed(lambda: c.request('fork', source='long', bot='tip', workspace=str(out))['result'])
    _, fork_old_ms = timed(lambda: c.request('fork', source='long', checkpoint=first_node, bot='old',
                                             workspace=str(out))['result'])
    _, tip_turn_ms = timed(lambda: turn(c, 'tip', 't', 'tip turn ' + FILLER))
    _, history_ms = timed(lambda: turn(c, 'long', 'read', 'history:1'))
    read = c.request('result', bot='long', turn=turn(c, 'long', 'read2', 'history:1'))['result']['text']
    rss_peak = daemon.memory_info().rss
    c.request('shutdown')
    c.close()
    server.shutdown()
    disk = sum(p.stat().st_size for p in out.glob('state.sqlite*'))
    return dict(schema='long_history_v2', created_at=datetime.now(timezone.utc).isoformat(),
                binary_sha256=file_hash(binary), stored_items=seeded_turns * 2, seeded_turns=seeded_turns,
                seed_seconds=seed_seconds, store_bytes=disk, context_bytes=context_bytes, context_items=context_items,
                request=request, startup_ms=startup_ms, resume_op_ms=resume_only_ms, turn_ms=turn_ms,
                fork_head_ms=fork_head_ms, fork_first_checkpoint_ms=fork_old_ms, forked_turn_ms=tip_turn_ms,
                history_read_turn_ms=history_ms, history_read_ok='seed 0' in read,
                daemon_rss_mib=dict(after_resume=round(rss_after_resume / 2**20, 2),
                                    after_turn=round(rss_after_turn / 2**20, 2), end=round(rss_peak / 2**20, 2)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--items', type=int, default=10000)
    parser.add_argument('--context-bytes', type=int, default=64 * 1024)
    parser.add_argument('--context-items', type=int, default=256)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    out = args.out.resolve()
    if Path.cwd() != root or not out.is_relative_to(root / '.local') or out.exists():
        parser.error('run from the repository root with a new output directory under .local')
    out.mkdir(parents=True)
    result = run(args.binary.resolve(), out, args.items, args.context_bytes, args.context_items)
    (out / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result))
    return 0 if result['history_read_ok'] else 1


if __name__ == '__main__':
    sys.exit(main())
