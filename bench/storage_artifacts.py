"""Matched mixed-turn artifact screen: varied text and incompressible ASCII.

Eight shell bots emit 1 MiB while eight text bots run ordinary turns. Retain
four turns per bot. Verify every byte of each bot's last artifact via bounded
protocol pages, and exact source data through fork and restart. No paid calls.
"""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import random
import sqlite3
import threading
import time

import psutil
from .events import percentiles
from .runtime_client import Client
from .storage_corpus import synthetic
from .storage_profile import profile
from .synthetic_model import start
from .targets import file_hash


def trial(binary, directory, fixture):
    directory.mkdir()
    (directory / 'output.txt').write_bytes(fixture)
    store = directory / 'state.sqlite'
    server, url = start()
    client = None
    stop = threading.Event()
    rss = []
    sampler = None
    try:
        client = Client(binary, store, url, 'shell', extra=('--retain-turns', '4', '--context-bytes', '524288'))
        process = psutil.Process(client.process.pid)
        def sample():
            while not stop.wait(.01):
                rss.append(process.memory_info().rss)
        sampler = threading.Thread(target=sample, daemon=True)
        sampler.start()
        for n in range(16):
            assert 'result' in client.request('create', bot=f'b{n}', workspace=str(directory))
        cpu_start = sum(process.cpu_times()[:2])
        at = time.monotonic()
        text_ms, shell_ms = [], []
        latest = {}
        for round in range(16):
            pending = []
            for n in range(16):
                prompt = f'shell:cat output.txt # {round}' if n < 8 else f'ordinary {round}'
                submitted = time.monotonic()
                result = client.request('submit', bot=f'b{n}', request_id=f'r{round}', prompt=prompt)
                pending.append((n, result['result']['turn'], submitted))
            for n, turn, submitted in pending:
                event = client.finished(turn)
                assert event['data']['status'] == 'completed', event
                (shell_ms if n < 8 else text_ms).append((event['_received_at']-submitted)*1000)
                latest[n] = turn
            client.saved.clear()
        wall = time.monotonic()-at
        cpu = sum(process.cpu_times()[:2])-cpu_start
        stats = client.request('stats')['result']
        # Partial read latency includes the protocol round trip. Check arbitrary
        # late offsets, not only the start of a compressed block.
        rng = random.Random(7)
        latencies = []
        for _ in range(128):
            n = rng.randrange(8)
            offset = rng.randrange(len(fixture)-65536)
            at = time.monotonic()
            page = client.request('artifact', bot=f'b{n}', turn=latest[n], call_id='sh-1',
                                  stream='stdout', offset=offset, limit=4096)['result']
            latencies.append((time.monotonic()-at)*1000)
            assert page['text'].encode() == fixture[offset:offset+4096]
        for n in range(8):
            actual = bytearray()
            while len(actual) < len(fixture):
                page = client.request('artifact', bot=f'b{n}', turn=latest[n], call_id='sh-1',
                                      stream='stdout', offset=len(actual), limit=65536)['result']
                actual.extend(page['text'].encode())
                assert page['next_offset'] == len(actual)
            assert actual == fixture
        stop.set()
        sampler.join()
        client.close()
        client = None
        attribution = profile(store)
        # Transcript bytes are independent of physical artifact encoding.
        with sqlite3.connect(store) as db:
            transcript = hashlib.sha256()
            for n in range(16):
                for (item,) in db.execute('''WITH RECURSIVE chain(id,parent,depth,item) AS (
                    SELECT id,parent,depth,item FROM nodes WHERE id=(SELECT head FROM bots WHERE name=?)
                    UNION ALL SELECT n.id,n.parent,n.depth,n.item FROM nodes n JOIN chain c ON n.id=c.parent)
                    SELECT item FROM chain ORDER BY depth''', (f'b{n}',)):
                    transcript.update(len(item).to_bytes(8, 'little'))
                    transcript.update(item)
        result = dict(cpu_seconds=cpu, wall_seconds=wall, text_ms=percentiles(text_ms),
                      shell_ms=percentiles(shell_ms), page_ms=percentiles(latencies),
                      peak_rss_bytes=max(rss), storage=attribution, stats=stats,
                      transcript_sha256=transcript.hexdigest(), completed_turns=256)
        client = Client(binary, store, url, 'shell')
        fork = client.request('fork', source='b0', bot='fork', workspace=str(directory))
        assert 'result' in fork, fork
        page = client.request('artifact', bot='fork', turn=latest[0], call_id='sh-1',
                              stream='stdout', offset=700001, limit=4096)['result']
        assert page['text'].encode() == fixture[700001:704097]
        assert 'result' in client.request('delete', bot='fork')
        return result
    finally:
        stop.set()
        if sampler:
            sampler.join()
        if client:
            client.close(kill=True)
        server.shutdown()
        server.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', required=True, type=Path)
    parser.add_argument('--after', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    binaries = {k: getattr(args, k).resolve() for k in ('before', 'after')}
    results = {'schema': 'storage_artifacts_v1', 'binaries': {k: file_hash(v) for k,v in binaries.items()}, 'runs': []}
    fixtures = {
        'varied': list(synthetic('varied', 4))[-1][1],
        'entropy': base64.b64encode(random.Random(7).randbytes(786432)),
    }
    for shape, fixture in fixtures.items():
        expected = None
        for pair in range(4):
            for label in (('before', 'after') if pair % 2 == 0 else ('after', 'before')):
                result = trial(binaries[label], out / f'{shape}-{pair}-{label}', fixture)
                expected = expected or result['transcript_sha256']
                assert result['transcript_sha256'] == expected
                result.update(shape=shape, pair=pair, label=label, warmup=pair==0)
                results['runs'].append(result)
                (out / 'result.json').write_text(json.dumps(results, indent=2)+'\n')
                print(json.dumps({k: result[k] for k in ('shape','pair','label','cpu_seconds','text_ms','shell_ms','page_ms','peak_rss_bytes')}), flush=True)


if __name__ == '__main__':
    main()
