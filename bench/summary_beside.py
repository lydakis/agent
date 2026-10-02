"""Turn wall time for one long synthetic turn whose summaries run before its
calls (one binary) or beside them (another), against the runtime tests'
model fixture with every work call delayed WORK seconds and every summary
SUMMARY seconds. Run from the repository root:

    .local/venv/bin/python -m bench.summary_beside BEFORE AFTER \\
        --work 1.0 --summary 4.0 --prompt long:150x40 --context-bytes 65536

Synthetic only: no credentials, no model calls; stores go under .local/."""
import argparse
import http.server
import io
import json
from pathlib import Path
import queue
import statistics
import tempfile
import threading
import time

from bench.runtime_client import Client
from tests.test_runtime import Model, is_summary


def run(binary, prompt, context_bytes, work, summary):
    class Slow(Model):
        def do_POST(self):
            body = self.rfile.read(int(self.headers['Content-Length']))
            time.sleep(summary if is_summary(json.loads(body)) else work)
            self.rfile = io.BytesIO(body)
            return Model.do_POST(self)

    class Server(http.server.ThreadingHTTPServer):
        request_queue_size = 128
    model = Server(('127.0.0.1', 0), Slow)
    model.requests, model.daemon_threads = queue.Queue(), True
    threading.Thread(target=model.serve_forever, daemon=True).start()
    root = Path('.local').resolve()
    root.mkdir(exist_ok=True)
    try:
        with tempfile.TemporaryDirectory(dir=root) as workspace:
            client = Client(Path(binary), Path(workspace) / 'state.sqlite',
                            f'http://127.0.0.1:{model.server_port}/v1', 'shell,read',
                            settings={'context_bytes': context_bytes})
            try:
                client.request('create', bot='Bob', workspace=workspace, tools=['shell', 'read'],
                               compaction_instructions='Summarize.')
                started = time.monotonic()
                turn = client.request('submit', bot='Bob', request_id='1', prompt=prompt)['result']['turn']
                ended = client.finished(turn, timeout=3600)
                wall = time.monotonic() - started
                if ended['data']['status'] != 'completed':
                    raise RuntimeError(ended['data'].get('error'))
                events, after = [], 0
                while page := client.request('events', bot='Bob', after=after, limit=256)['result']['events']:
                    events += page
                    after = page[-1]['cursor']
            finally:
                client.close()
    finally:
        model.shutdown()
        model.server_close()
    requests = []
    while not model.requests.empty():
        requests.append(model.requests.get())
    work_requests = [r for r in requests if not is_summary(r)]
    compacted = [e['data'] for e in events if e['event'] == 'compacted']
    return {'wall_s': wall, 'summaries': len(requests) - len(work_requests), 'calls': len(work_requests),
            'beside': sum(bool((c.get('request') or {}).get('beside')) for c in compacted),
            'max_input_bytes': max(len(json.dumps(r['input'])) for r in work_requests),
            'work_input_bytes': sum(len(json.dumps(r['input'])) for r in work_requests)}


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('binaries', nargs='+')
    parser.add_argument('--work', type=float, default=1.0)
    parser.add_argument('--summary', type=float, default=4.0)
    parser.add_argument('--prompt', default='long:150x40')
    parser.add_argument('--context-bytes', type=int, default=65536)
    parser.add_argument('--runs', type=int, default=1)
    args = parser.parse_args()
    for binary in args.binaries:
        rows = [run(binary, args.prompt, args.context_bytes, args.work, args.summary) for _ in range(args.runs)]
        walls = [row['wall_s'] for row in rows]
        print(json.dumps({'binary': binary, 'wall_s_median': round(statistics.median(walls), 2),
                          'wall_s_range': [round(min(walls), 2), round(max(walls), 2)],
                          **{key: [row[key] for row in rows] for key in rows[0] if key != 'wall_s'}}))


if __name__ == '__main__':
    main()
