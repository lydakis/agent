"""What a burst of messages costs the agent they all go to.

One agent, `lead`, hears from N others at once, as when N helpers it
started end together and each says so with `send_message` (a steer); then
from N over one second, while its turns run; then from N spread out, each
after the lead's last turn has ended. A synthetic Responses model answers
every request after LATENCY ms. Reports the lead's model requests, the
input it sent (bytes and items), and the time from the first message until
a request had read all of them.

    .local/venv/bin/python -m bench.burst_messages --out .local/bench/burst
"""
import argparse
import http.server
import json
import re
import threading
import time
from pathlib import Path

from .runtime_client import Client


class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers['Content-Length'])
        request = json.loads(self.rfile.read(length))
        words = json.dumps(request['input'])
        self.server.seen.append({'at': time.monotonic(), 'bytes': length, 'items': len(request['input']),
                                 'heard': set(re.findall(r'helper (\d+) is done', words))})
        time.sleep(self.server.latency)
        output = [{'type': 'message', 'role': 'assistant',
                   'content': [{'type': 'output_text', 'text': 'noted'}]}]
        events = [{'type': 'response.created', 'response': {'id': 'r'}},
                  {'type': 'response.output_text.delta', 'delta': 'noted'},
                  {'type': 'response.completed', 'response': {'status': 'completed', 'output': output,
                   'usage': {'input_tokens': 1, 'output_tokens': 1, 'input_tokens_details': {'cached_tokens': 0}}}}]
        body = b''.join(('data: ' + json.dumps(e) + '\n\n').encode() for e in events)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def message(n):
    return {'op': 'submit', 'bot': 'lead', 'request_id': f'h{n}', 'delivery': 'steer',
            'prompt': f'helper {n} is done: ' + 'result ' * 40}


def run(binary, out, count, latency, mode):
    out.mkdir(parents=True)
    out = out.resolve()
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
    server.seen, server.latency, server.daemon_threads = [], latency / 1000, True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    client = Client(binary, out / 'state.sqlite', f'http://127.0.0.1:{server.server_port}/v1', 'echo')
    try:
        assert 'result' in client.request('create', bot='lead', workspace=str(out))

        def idle(timeout=300):
            # Idle: no turn running for a few looks in a row, so none of
            # the messages waits to start one.
            deadline, quiet = time.monotonic() + timeout, 0
            while time.monotonic() < deadline:
                state = client.request('inspect', bot='lead')['result']
                quiet = quiet + 1 if state.get('running_turn') is None else 0
                if quiet == 4:
                    return
                time.sleep(.05)
            raise AssertionError('the lead never went idle')

        began = time.monotonic()
        if mode == 'spread':
            for n in range(count):
                client.request(**message(n))
                idle()
        elif mode == 'second':
            for n in range(count):
                client.process.stdin.write(json.dumps({'id': 10_000 + n, **message(n)}) + '\n')
                client.process.stdin.flush()
                time.sleep(1 / count)
            idle()
        else:
            # Written at once, as N helpers ending together send them.
            lines = ''.join(json.dumps({'id': 10_000 + n, **message(n)}) + '\n' for n in range(count))
            client.process.stdin.write(lines)
            client.process.stdin.flush()
            time.sleep(.2)
            idle()
        ended = time.monotonic()
    finally:
        client.close()
        server.shutdown()
    heard, all_heard = set(), None
    for request in server.seen:
        heard |= request['heard']
        if all_heard is None and len(heard) == count:
            all_heard = round((request['at'] - began) * 1000)
    return dict(mode=mode, messages=count, latency_ms=latency,
                model_requests=len(server.seen),
                input_kib=round(sum(r['bytes'] for r in server.seen) / 1024, 1),
                input_items=sum(r['items'] for r in server.seen),
                ms_until_all_read=all_heard, ms_until_idle=round((ended - began) * 1000))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--messages', type=int, default=100)
    parser.add_argument('--latency', type=int, default=300, help='ms the model takes to answer')
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    rows = [run(args.binary.resolve(), args.out / mode, args.messages, args.latency, mode)
            for mode in ('burst', 'second', 'spread')]
    (args.out / 'result.json').write_text(json.dumps(rows, indent=2) + '\n')
    for row in rows:
        print(json.dumps(row), flush=True)


if __name__ == '__main__':
    main()
