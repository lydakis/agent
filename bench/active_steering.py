"""Matched active-steering screen using a gated, history-checking local provider.

Every round queues the same steers before releasing any provider response.
All steers must fit both implementations: this measures accounting cost, not
the different overload behaviors. Only daemon CPU/RSS are charged to the target.
"""
import argparse
import hashlib
import http.server
import json
import math
import platform
import queue
import tempfile
import threading
import time
from pathlib import Path

import psutil

from .runtime_client import Client
from .targets import file_hash


def user(text):
    return {'role': 'user', 'content': [{'type': 'input_text', 'text': text}]}


def reply(round_id, count):
    return [{'type': 'message', 'role': 'assistant',
             'content': [{'type': 'output_text', 'text': f'reply:{round_id}:{n}'}]}
            for n in range(count)]


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
        body = self.rfile.read(int(self.headers['Content-Length']))
        request = json.loads(body)
        gate = queue.Queue()
        self.server.arrivals.put((request['input'], len(body), gate, time.monotonic()))
        output = gate.get(timeout=60)
        event = {'type': 'response.completed', 'response': {
            'status': 'completed', 'output': output,
            'usage': {'input_tokens': 1, 'output_tokens': 1}}}
        delta = {'type': 'response.output_text.delta',
                 'delta': ''.join(item['content'][0]['text'] for item in output)}
        payload = ''.join('data: ' + json.dumps(e) + '\n\n' for e in (delta, event)).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def percentile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


def run(binary, *, bots=8, rounds=20, steers=40, output_items=1):
    # Stay below both the window target and the model-round/output bounds.
    if not (1 <= bots <= 32 and 1 <= rounds <= 24 and 1 <= steers <= 64
            and 1 <= output_items <= 48
            and 1 + rounds * (steers + output_items) + output_items <= 3072):
        raise ValueError('workload must fit the shared context and round contract')
    class Server(http.server.ThreadingHTTPServer):
        daemon_threads = True
        request_queue_size = 128
    server = Server(('127.0.0.1', 0), Model)
    server.arrivals = queue.Queue()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    stop, samples, gates = threading.Event(), [], []
    sampler = None
    client = None
    try:
        with tempfile.TemporaryDirectory(prefix='active-steering-') as directory:
            client = Client(binary, Path(directory) / 'state.sqlite',
                            f'http://127.0.0.1:{server.server_port}/v1', tools='echo')
            process = psutil.Process(client.process.pid)
            expected = {}
            for n in range(bots):
                name = f'b{n}'
                assert 'result' in client.request('create', bot=name, workspace=directory)
                expected[name] = [user(name)]
            initial = client.request('stats')['result']['store']
            def sample():
                while not stop.is_set():
                    samples.append(process.memory_info().rss)
                    stop.wait(.005)
            sampler = threading.Thread(target=sample, daemon=True)
            sampler.start()
            cpu0 = process.cpu_times()
            started = time.monotonic()
            roots = {name: client.request('submit', bot=name, request_id='root',
                                         prompt=name)['result']['turn'] for name in expected}
            latency, byte_count, digest = [], 0, hashlib.sha256()
            releases = {}
            pending = []
            absorbed_count = 0
            for step in range(rounds + 1):
                arrivals = {}
                for _ in range(bots):
                    history, size, gate, arrived = server.arrivals.get(timeout=30)
                    gates.append(gate)
                    name = history[0]['content'][0]['text']
                    assert name not in arrivals, 'duplicate provider call'
                    assert history == expected[name], f'history mismatch at {name}/{step}'
                    if step:
                        latency.append((arrived - releases[name]) * 1000)
                    arrivals[name] = gate
                    byte_count += size
                for name, turn in pending:
                    outcome = client.finished(turn)['data']
                    assert outcome['status'] == 'steered' and outcome['into'] == roots[name]
                    absorbed_count += 1
                pending.clear()
                # All prior boundary events have now arrived. Keep controller
                # replay buffers out of subsequent boundary measurements.
                client.saved.clear()
                # Deterministic digest independent of provider arrival order.
                for name in sorted(expected):
                    digest.update(json.dumps(expected[name], sort_keys=True).encode())
                output = reply(step, output_items)
                for name in expected:
                    expected[name].extend(output)
                    if step < rounds:
                        for n in range(steers):
                            prompt = f'{step}:{n}:' + 's' * 256
                            response = client.request('submit', bot=name, request_id=f's{step}-{n}',
                                                      prompt=prompt, delivery='steer',
                                                      expected_turn=roots[name])
                            assert 'result' in response, response
                            result = response['result']
                            assert result['status'] == 'queued'
                            pending.append((name, result['turn']))
                            expected[name].append(user(prompt))
                for name, gate in arrivals.items():
                    releases[name] = time.monotonic()
                    gate.put(output)
            for name, turn in roots.items():
                assert client.finished(turn)['data']['status'] == 'completed'
            elapsed = time.monotonic() - started
            cpu1 = process.cpu_times()
            final = client.request('stats')['result']['store']
            stop.set()
            sampler.join()
            operations = {}
            for name, stats in final['operations'].items():
                before = initial['operations'].get(name, {})
                operations[name] = {k: stats[k] - before.get(k, 0)
                                    for k in ('count', 'queued_ms', 'ran_ms')}
            assert operations['absorb']['count'] == bots * rounds * (steers // 32 + 1)
            assert server.arrivals.empty(), 'unexpected extra model call'
            return {'daemon_cpu_s': cpu1.user + cpu1.system - cpu0.user - cpu0.system,
                    'daemon_peak_rss_mib': max(samples) / 2**20, 'wall_s': elapsed,
                    'boundary_p50_ms': percentile(latency, .5),
                    'boundary_p95_ms': percentile(latency, .95),
                    'provider_calls': bots * (rounds + 1), 'request_bytes': byte_count,
                    'absorbed_steers': absorbed_count, 'history_sha256': digest.hexdigest(),
                    'operations': operations}
    finally:
        stop.set()
        if sampler:
            sampler.join()
        for gate in gates:
            gate.put([])
        if client:
            client.close(kill=True)
        server.shutdown()
        server.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--bots', type=int, default=8)
    parser.add_argument('--rounds', type=int, default=20)
    parser.add_argument('--steers', type=int, default=40)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    binaries = {'before': args.before.resolve(), 'after': args.after.resolve()}
    report = {'schema': 1, 'host': platform.platform(),
              'binary_sha256': {k: file_hash(v) for k, v in binaries.items()},
              'workload': {'bots': args.bots, 'rounds': args.rounds, 'steers': args.steers},
              'warmups': [], 'runs': []}
    for items in (1, 48):
        contract = None
        for index, label in enumerate(['before', 'after'] + ['before', 'after', 'after', 'before'] * 2):
            result = run(binaries[label], bots=args.bots, rounds=args.rounds,
                         steers=args.steers, output_items=items)
            checked = {k: result[k] for k in ('provider_calls', 'request_bytes',
                                            'absorbed_steers', 'history_sha256')}
            if contract is None:
                contract = checked
            assert checked == contract, 'unmatched workload'
            record = {'binary': label, 'output_items': items, **result}
            report['warmups' if index < 2 else 'runs'].append(record)
            (args.out / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps({k: v for k, v in record.items() if k != 'operations'}), flush=True)


if __name__ == '__main__':
    main()
